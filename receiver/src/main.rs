use image::codecs::jpeg::JpegDecoder;
use image::{ColorType, ImageDecoder};
use quinn::{Endpoint, ServerConfig};
use rcgen::{Certificate as RcgenCertificate, CertificateParams, DistinguishedName};
use rustls::{Certificate as RustlsCertificate, PrivateKey as RustlsPrivateKey};
use softbuffer::{Context, Surface};
use std::env;
use std::io::Cursor;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

struct ReceiverApp {
    frame_rx: UnboundedReceiver<Vec<u8>>,
    window_attributes: WindowAttributes,
    window: Option<Rc<Window>>,
    render_target: Option<(Context<Rc<Window>>, Surface<Rc<Window>, Rc<Window>>)>,
    current_image: Option<Vec<u32>>,
    width: u32,
    height: u32,
    frame_dirty: bool,
    verbose: bool,
    locked_size: Option<(u32, u32)>,
}

const DEFAULT_WINDOW_WIDTH: u32 = 960;
const DEFAULT_WINDOW_HEIGHT: u32 = 540;

impl ReceiverApp {
    fn new(frame_rx: UnboundedReceiver<Vec<u8>>) -> Self {
        let window_attributes = Window::default_attributes()
            .with_title("Rust Screen Mirror Receiver")
            .with_inner_size(LogicalSize::new(
                DEFAULT_WINDOW_WIDTH as f64,
                DEFAULT_WINDOW_HEIGHT as f64,
            ));
        Self {
            frame_rx,
            window_attributes,
            window: None,
            render_target: None,
            current_image: None,
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
            frame_dirty: false,
            verbose: env::var("VM_VERBOSE").map(|v| v != "0").unwrap_or(false),
            locked_size: Some((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)),
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        match event_loop.create_window(self.window_attributes.clone()) {
            Ok(new_window) => {
                let window_rc = Rc::new(new_window);
                match Context::new(window_rc.clone()) {
                    Ok(context) => match Surface::new(&context, window_rc.clone()) {
                        Ok(surface) => {
                            self.render_target = Some((context, surface));
                            self.window = Some(window_rc.clone());
                            let placeholder_len =
                                (self.width as usize).saturating_mul(self.height as usize);
                            if placeholder_len > 0 {
                                self.current_image = Some(vec![0xFF_00_0000; placeholder_len]);
                                self.frame_dirty = true;
                            }
                            self.locked_size = Some((self.width, self.height));
                            let _ = window_rc.request_inner_size(LogicalSize::new(
                                self.width as f64,
                                self.height as f64,
                            ));
                            window_rc.request_redraw();
                        }
                        Err(err) => {
                            eprintln!("Surface creation failed: {err}");
                            event_loop.exit();
                        }
                    },
                    Err(err) => {
                        eprintln!("Context creation failed: {err}");
                        event_loop.exit();
                    }
                }
            }
            Err(err) => {
                eprintln!("Window creation failed: {err}");
                event_loop.exit();
            }
        }
    }

    fn drain_frames(&mut self) {
        use tokio::sync::mpsc::error::TryRecvError;
        loop {
            match self.frame_rx.try_recv() {
                Ok(data) => self.process_frame(data),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    eprintln!("Frame stream disconnected");
                    break;
                }
            }
        }
    }

    fn process_frame(&mut self, data: Vec<u8>) {
        match JpegDecoder::new(Cursor::new(data)) {
            Ok(decoder) => self.handle_decoded_frame(decoder),
            Err(err) => eprintln!("Failed to create JPEG decoder: {err}"),
        }
    }

    fn handle_decoded_frame<D>(&mut self, decoder: D)
    where
        D: ImageDecoder,
    {
        let (img_width, img_height) = decoder.dimensions();
        let color_type = decoder.color_type();
        let bytes_per_pixel = color_type.bytes_per_pixel() as usize;
        let mut raw = vec![0; img_width as usize * img_height as usize * bytes_per_pixel];

        if decoder.read_image(&mut raw).is_ok() {
            if let Some(pixels) = convert_to_pixels(&raw, color_type) {
                let new_size = (img_width, img_height);
                let size_changed = self.locked_size.map_or(true, |size| size != new_size);
                self.width = img_width;
                self.height = img_height;
                if size_changed {
                    self.locked_size = Some(new_size);
                }
                self.current_image = Some(pixels);
                self.frame_dirty = true;
                if self.verbose {
                    println!("Decoded frame: {img_width}x{img_height} ({:?})", color_type);
                }
                if let Some(window) = &self.window {
                    if size_changed {
                        let logical_size = LogicalSize::new(img_width as f64, img_height as f64);
                        let _ = window.request_inner_size(logical_size);
                    }
                    window.request_redraw();
                }
            } else {
                eprintln!("Unsupported color type {color_type:?}, frame dropped");
            }
        } else {
            eprintln!("Failed reading pixels for frame {img_width}x{img_height}");
        }
    }

    fn render(&mut self) {
        let Some((_, surface)) = self.render_target.as_mut() else {
            return;
        };
        let Some(pixels) = self.current_image.as_ref() else {
            return;
        };
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let window_size = window.inner_size();
        if window_size.width == 0 || window_size.height == 0 {
            return;
        }
        if let Some((locked_w, locked_h)) = self.locked_size {
            if window_size.width != locked_w || window_size.height != locked_h {
                let logical_size = LogicalSize::new(locked_w as f64, locked_h as f64);
                let _ = window.request_inner_size(logical_size);
                window.request_redraw();
                return;
            }
        }
        let Some(width_nz) = NonZeroU32::new(window_size.width) else {
            return;
        };
        let Some(height_nz) = NonZeroU32::new(window_size.height) else {
            return;
        };

        if surface.resize(width_nz, height_nz).is_err() {
            eprintln!(
                "Surface resize failed for window size {}x{}",
                window_size.width, window_size.height
            );
            return;
        }

        if let Ok(mut buffer) = surface.buffer_mut() {
            let dst_width = window_size.width as usize;
            let dst_height = window_size.height as usize;
            let src_width = self.width as usize;
            let src_height = self.height as usize;

            if self.verbose {
                println!(
                    "Rendering frame {}x{} into buffer {}x{} (buffer len {}, pixels len {})",
                    src_width,
                    src_height,
                    dst_width,
                    dst_height,
                    buffer.len(),
                    pixels.len()
                );
            }

            if buffer.len() == pixels.len() && dst_width == src_width && dst_height == src_height {
                buffer.copy_from_slice(pixels);
            } else if src_width == 0 || src_height == 0 {
                return;
            } else {
                if self.verbose {
                    println!(
                        "Scaling frame {}x{} -> {}x{}",
                        src_width, src_height, dst_width, dst_height
                    );
                }
                for y in 0..dst_height {
                    let src_y = (y * src_height) / dst_height;
                    let src_row = src_y * src_width;
                    let dst_row = y * dst_width;
                    for x in 0..dst_width {
                        let src_x = (x * src_width) / dst_width;
                        buffer[dst_row + x] = pixels[src_row + src_x];
                    }
                }
            }
            let _ = buffer.present();
            self.frame_dirty = false;
            if self.verbose {
                println!(
                    "Presented frame (source {}x{}, window {}x{})",
                    src_width, src_height, dst_width, dst_height
                );
            }
        }
    }
}

impl ApplicationHandler<()> for ReceiverApp {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::Init) {
            event_loop.set_control_flow(ControlFlow::Poll);
            self.ensure_window(event_loop);
        }
        self.drain_frames();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_window(event_loop);
        self.drain_frames();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.drain_frames();

        let Some(window) = &self.window else {
            return;
        };

        if window_id != window.id() {
            return;
        }

        match event {
            WindowEvent::Resized(size) => {
                if let Some((locked_w, locked_h)) = self.locked_size {
                    if size.width != locked_w || size.height != locked_h {
                        let logical_size = LogicalSize::new(locked_w as f64, locked_h as f64);
                        let _ = window.request_inner_size(logical_size);
                    }
                } else {
                    self.locked_size = Some((size.width, size.height));
                    self.width = size.width;
                    self.height = size.height;
                }
                self.frame_dirty = true;
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.drain_frames();
        if self.frame_dirty {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

fn main() {
    let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_quic_server(frame_tx);
    println!("Listening for QUIC connections on 0.0.0.0:5000...");

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = ReceiverApp::new(frame_rx);

    event_loop.run_app(&mut app).expect("event loop run failed");
}

fn spawn_quic_server(frame_tx: UnboundedSender<Vec<u8>>) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        runtime.block_on(async move {
            if let Err(err) = run_quic_server(frame_tx).await {
                eprintln!("QUIC server error: {err}");
            }
        });
    });
}

async fn run_quic_server(
    frame_tx: UnboundedSender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = "0.0.0.0:5000".parse()?;
    let server_config = make_server_config()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    while let Some(connecting) = endpoint.accept().await {
        let tx_cloned = frame_tx.clone();
        tokio::spawn(async move {
            match connecting.await {
                Ok(connection) => {
                    if let Err(err) = handle_connection(connection, tx_cloned).await {
                        eprintln!("Connection error: {err}");
                    }
                }
                Err(err) => eprintln!("Incoming connection failed: {err}"),
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    connection: quinn::Connection,
    frame_tx: UnboundedSender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        match connection.accept_uni().await {
            Ok(recv) => {
                let tx = frame_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) = process_stream(recv, tx).await {
                        eprintln!("Stream handling error: {err}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => break,
            Err(err) => {
                eprintln!("Failed to accept stream: {err}");
                break;
            }
        }
    }
    Ok(())
}

async fn process_stream(
    mut recv: quinn::RecvStream,
    frame_tx: UnboundedSender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    let mut data = vec![0u8; frame_len];
    recv.read_exact(&mut data).await?;
    if frame_tx.send(data).is_err() {
        eprintln!("Receiver window dropped frame channel");
    }
    Ok(())
}

fn make_server_config() -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cert = generate_self_signed_cert()?;
    let mut server_config = ServerConfig::with_single_cert(cert.0, cert.1)?;
    Arc::get_mut(&mut server_config.transport)
        .expect("transport config not shared")
        .max_concurrent_uni_streams(1024_u32.into());
    Ok(server_config)
}

fn generate_self_signed_cert(
) -> Result<(Vec<rustls::Certificate>, rustls::PrivateKey), Box<dyn std::error::Error + Send + Sync>>
{
    let mut params = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]);
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "virtual-monitor");
    params.distinguished_name = dn;
    let cert = RcgenCertificate::from_params(params)?;
    let cert_der = cert.serialize_der()?;
    let key_der = cert.serialize_private_key_der();
    let cert_chain = vec![RustlsCertificate(cert_der)];
    let priv_key = RustlsPrivateKey(key_der);
    Ok((cert_chain, priv_key))
}

fn convert_to_pixels(raw: &[u8], color_type: ColorType) -> Option<Vec<u32>> {
    match color_type {
        ColorType::Rgb8 => Some(
            raw.chunks_exact(3)
                .map(|p| {
                    0xFF_00_0000 | ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | (p[2] as u32)
                })
                .collect(),
        ),
        ColorType::Rgba8 => Some(
            raw.chunks_exact(4)
                .map(|p| {
                    ((p[3] as u32) << 24)
                        | ((p[0] as u32) << 16)
                        | ((p[1] as u32) << 8)
                        | (p[2] as u32)
                })
                .collect(),
        ),
        ColorType::L8 => Some(
            raw.iter()
                .map(|&v| {
                    let v = v as u32;
                    0xFF_00_0000 | (v << 16) | (v << 8) | v
                })
                .collect(),
        ),
        _ => None,
    }
}
