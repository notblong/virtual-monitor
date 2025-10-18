use image::codecs::jpeg::JpegDecoder;
use image::{ColorType, ImageDecoder};
use softbuffer::{Context, Surface};
use std::collections::HashMap;
use std::convert::TryInto;
use std::env;
use std::io::Cursor;
use std::net::UdpSocket;
use std::num::NonZeroU32;
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

struct FrameBuffer {
    chunks: HashMap<u16, Vec<u8>>,
    total_chunks: u16,
}

struct ReceiverApp {
    socket: UdpSocket,
    frames: HashMap<u32, FrameBuffer>,
    buf: [u8; 65_507],
    window_attributes: WindowAttributes,
    window: Option<Rc<Window>>,
    render_target: Option<(Context<Rc<Window>>, Surface<Rc<Window>, Rc<Window>>)>,
    current_image: Option<Vec<u32>>,
    width: u32,
    height: u32,
    frame_dirty: bool,
    verbose: bool,
}

impl ReceiverApp {
    fn new(socket: UdpSocket) -> Self {
        let window_attributes =
            Window::default_attributes().with_title("Rust Screen Mirror Receiver");
        Self {
            socket,
            frames: HashMap::new(),
            buf: [0u8; 65_507],
            window_attributes,
            window: None,
            render_target: None,
            current_image: None,
            width: 800,
            height: 600,
            frame_dirty: false,
            verbose: env::var("VM_VERBOSE").map(|v| v != "0").unwrap_or(false),
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

    fn drain_socket(&mut self) {
        while let Ok((len, _src)) = self.socket.recv_from(&mut self.buf) {
            if len < 8 {
                eprintln!("Discarded packet smaller than header (len={len})");
                continue;
            }

            let frame_id = u32::from_be_bytes(self.buf[0..4].try_into().unwrap());
            let chunk_index = u16::from_be_bytes(self.buf[4..6].try_into().unwrap());
            let total_chunks = u16::from_be_bytes(self.buf[6..8].try_into().unwrap());
            let data = &self.buf[8..len];

            let frame = self.frames.entry(frame_id).or_insert_with(|| FrameBuffer {
                chunks: HashMap::new(),
                total_chunks,
            });
            frame.chunks.insert(chunk_index, data.to_vec());
            let stored_chunks = frame.chunks.len();

            if self.verbose {
                println!(
                    "Frame {frame_id}: stored chunk {chunk_index}/{} (have {stored_chunks}/{total_chunks})",
                    total_chunks.saturating_sub(1)
                );
            }

            if stored_chunks == 1 && self.verbose {
                println!(
                    "Started frame {frame_id} (expecting {total_chunks} chunks, first chunk {chunk_index})"
                );
            }

            if stored_chunks as u16 == frame.total_chunks {
                let mut jpeg_data = Vec::new();
                for i in 0..frame.total_chunks {
                    if let Some(chunk) = frame.chunks.get(&i) {
                        jpeg_data.extend_from_slice(chunk);
                    } else {
                        eprintln!(
                            "Frame {frame_id} missing chunk {i}, assembled data may be incomplete"
                        );
                    }
                }
                if self.verbose {
                    println!(
                        "Frame {frame_id} assembled with {} chunks ({} bytes total)",
                        frame.total_chunks,
                        jpeg_data.len()
                    );
                }

                if let Ok(decoder) = JpegDecoder::new(Cursor::new(jpeg_data)) {
                    self.handle_decoded_frame(decoder);
                } else {
                    eprintln!("Failed to create JPEG decoder for frame {frame_id}");
                }

                self.frames.remove(&frame_id);
            }
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
                let size_changed = self.width != img_width || self.height != img_height;
                self.width = img_width;
                self.height = img_height;
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
        self.drain_socket();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_window(event_loop);
        self.drain_socket();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.drain_socket();

        let Some(window) = &self.window else {
            return;
        };

        if window_id != window.id() {
            return;
        }

        match event {
            WindowEvent::Resized(_size) => {
                self.frame_dirty = true;
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.drain_socket();
        if self.frame_dirty {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

fn main() {
    let socket = UdpSocket::bind("0.0.0.0:5000").expect("bind failed");
    socket
        .set_nonblocking(true)
        .expect("failed to set nonblocking");
    println!("Listening on UDP port 5000...");

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = ReceiverApp::new(socket);

    event_loop.run_app(&mut app).expect("event loop run failed");
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
