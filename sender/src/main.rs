use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ColorType, RgbImage};
use quinn::{ClientConfig, Endpoint};
use rustls::client::{
    ClientConfig as RustlsClientConfig, ServerCertVerified, ServerCertVerifier, ServerName,
};
use rustls::Error as RustlsError;
use screenshots::Screen;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{env, error::Error};
use tokio::task;
use tokio::time::sleep;

const DEFAULT_MAX_FRAME_WIDTH: u32 = 960;
const DEFAULT_MAX_FRAME_HEIGHT: u32 = 540;
const DEFAULT_MAX_FPS: f32 = 30.0;
const DEFAULT_JPEG_QUALITY: u8 = 60;
const DEFAULT_RECEIVER_IP: &str = "127.0.0.1";
const USAGE: &str = "\
Usage: sender [options] [receiver_ip]

Options:
  --receiver <ip>          Receiver IPv4/IPv6 address (default 127.0.0.1 or first positional)
  --max-width <pixels>     Maximum frame width (default 960)
  --max-height <pixels>    Maximum frame height (default 540)
  --max-fps <fps>          Target frames per second (default 30.0)
  --jpeg-quality <1-100>   JPEG quality percentage (default 60)
  --verbose / -v           Enable verbose logging (can also set VM_VERBOSE=1)
  --no-verbose / -q        Disable verbose logging
  --help / -h              Show this message

Examples:
  cargo run --release -- --receiver 192.168.1.50 --max-width 1600 --max-fps 45
  cargo run --release -- --jpeg-quality 30
";

#[derive(Clone)]
struct SenderConfig {
    receiver: String,
    max_width: u32,
    max_height: u32,
    max_fps: f32,
    jpeg_quality: u8,
    verbose: bool,
}

impl SenderConfig {
    fn from_env() -> Result<Self, String> {
        let mut receiver: Option<String> = None;
        let mut max_width: Option<u32> = None;
        let mut max_height: Option<u32> = None;
        let mut max_fps: Option<f32> = None;
        let mut jpeg_quality: Option<u8> = None;
        let mut verbose = env::var("VM_VERBOSE").map(|v| v != "0").unwrap_or(false);
        let mut positional: Vec<String> = Vec::new();

        let raw_args: Vec<String> = env::args().skip(1).collect();
        let mut i = 0;
        while i < raw_args.len() {
            let arg = &raw_args[i];
            if let Some(flag) = arg.strip_prefix("--") {
                match flag {
                    "receiver" | "receiver-ip" | "ip" => {
                        i += 1;
                        let value = raw_args
                            .get(i)
                            .ok_or_else(|| format!("Expected value after --{flag}"))?
                            .clone();
                        receiver = Some(value);
                    }
                    "max-width" | "width" => {
                        i += 1;
                        let value = raw_args
                            .get(i)
                            .ok_or_else(|| format!("Expected value after --{flag}"))?;
                        let parsed = value
                            .parse::<u32>()
                            .map_err(|_| format!("Invalid integer for --{flag}: {value}"))?;
                        if parsed == 0 {
                            return Err(format!("--{flag} must be greater than zero"));
                        }
                        max_width = Some(parsed);
                    }
                    "max-height" | "height" => {
                        i += 1;
                        let value = raw_args
                            .get(i)
                            .ok_or_else(|| format!("Expected value after --{flag}"))?;
                        let parsed = value
                            .parse::<u32>()
                            .map_err(|_| format!("Invalid integer for --{flag}: {value}"))?;
                        if parsed == 0 {
                            return Err(format!("--{flag} must be greater than zero"));
                        }
                        max_height = Some(parsed);
                    }
                    "max-fps" | "fps" => {
                        i += 1;
                        let value = raw_args
                            .get(i)
                            .ok_or_else(|| format!("Expected value after --{flag}"))?;
                        let fps = value
                            .parse::<f32>()
                            .map_err(|_| format!("Invalid float for --{flag}: {value}"))?;
                        if fps <= 0.0 {
                            return Err(format!("--{flag} must be greater than zero"));
                        }
                        max_fps = Some(fps);
                    }
                    "jpeg-quality" | "jpeg_quality" | "quality" => {
                        i += 1;
                        let value = raw_args
                            .get(i)
                            .ok_or_else(|| format!("Expected value after --{flag}"))?;
                        let quality = value
                            .parse::<u8>()
                            .map_err(|_| format!("Invalid integer for --{flag}: {value}"))?;
                        if quality == 0 || quality > 100 {
                            return Err("--jpeg-quality must be between 1 and 100".to_string());
                        }
                        jpeg_quality = Some(quality);
                    }
                    "verbose" => {
                        verbose = true;
                    }
                    "no-verbose" => {
                        verbose = false;
                    }
                    "help" => {
                        print_usage();
                        return Err(String::new());
                    }
                    other => {
                        return Err(format!("Unknown flag '--{other}'\n{USAGE}"));
                    }
                }
            } else if arg == "-v" {
                verbose = true;
            } else if arg == "-q" {
                verbose = false;
            } else if arg == "-h" {
                print_usage();
                return Err(String::new());
            } else {
                positional.push(arg.clone());
            }
            i += 1;
        }

        if receiver.is_none() {
            if let Some(pos0) = positional.first() {
                receiver = Some(pos0.clone());
            }
        }

        let receiver = receiver.unwrap_or_else(|| DEFAULT_RECEIVER_IP.to_string());

        Ok(Self {
            receiver,
            max_width: max_width.unwrap_or(DEFAULT_MAX_FRAME_WIDTH),
            max_height: max_height.unwrap_or(DEFAULT_MAX_FRAME_HEIGHT),
            max_fps: max_fps.unwrap_or(DEFAULT_MAX_FPS),
            jpeg_quality: jpeg_quality.unwrap_or(DEFAULT_JPEG_QUALITY),
            verbose,
        })
    }
}

fn print_usage() {
    println!("{USAGE}");
}

#[tokio::main]
async fn main() {
    let config = match SenderConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) if err.is_empty() => return,
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            return;
        }
    };

    let server_addr: SocketAddr = format!("{}:5000", config.receiver)
        .parse()
        .expect("invalid receiver address");
    let frame_interval = Duration::from_secs_f32(1.0 / config.max_fps);
    let config = Arc::new(config);

    let info_screen = Screen::from_point(0, 0).expect("no screen found");
    println!(
        "Streaming display {}x{} to {} (max frame size {}x{}, max {:.1} fps, JPEG quality {})",
        info_screen.display_info.width,
        info_screen.display_info.height,
        server_addr,
        config.max_width,
        config.max_height,
        config.max_fps,
        config.jpeg_quality
    );
    let mut endpoint =
        Endpoint::client("0.0.0.0:0".parse().unwrap()).expect("failed to create client endpoint");
    endpoint.set_default_client_config(make_client_config());

    let connecting = endpoint
        .connect(server_addr, "virtual-monitor")
        .expect("connect request failed");
    let connection = connecting
        .await
        .expect("failed to establish QUIC connection");
    println!("Connected to {server_addr}");

    let mut frame_id: u32 = 0;
    loop {
        let start = Instant::now();
        let cfg = Arc::clone(&config);
        let current_frame = frame_id;
        frame_id = frame_id.wrapping_add(1);

        let capture_result = task::spawn_blocking(move || capture_frame(&cfg, current_frame))
            .await
            .unwrap();

        match capture_result {
            Ok(frame) => {
                if let Err(err) = send_frame(&connection, &frame).await {
                    eprintln!("Failed to send frame {current_frame}: {err}");
                } else if config.verbose {
                    println!(
                        "Sent frame {current_frame}: {}x{} ({} bytes)",
                        frame.width,
                        frame.height,
                        frame.bytes.len()
                    );
                }
            }
            Err(err) => {
                eprintln!("Capture failed: {err}");
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            sleep(frame_interval - elapsed).await;
        }
    }
}

struct FrameData {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

fn capture_frame(
    config: &SenderConfig,
    frame_id: u32,
) -> Result<FrameData, Box<dyn Error + Send + Sync>> {
    let screen = Screen::from_point(0, 0).map_err(|err| format!("screen error: {err}"))?;
    let capture = screen
        .capture()
        .map_err(|err| format!("capture failed: {err}"))?;
    let width = capture.width();
    let height = capture.height();

    let rgba = capture.rgba();
    let mut rgb_buffer = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb_buffer.push(pixel[0]);
        rgb_buffer.push(pixel[1]);
        rgb_buffer.push(pixel[2]);
    }

    let rgb_image = RgbImage::from_raw(width, height, rgb_buffer).ok_or_else(|| {
        format!("Failed to build RGB image for frame {frame_id} ({width}x{height})")
    })?;

    let (frame_width, frame_height, frame_pixels) =
        if width > config.max_width || height > config.max_height {
            let scale_x = config.max_width as f32 / width as f32;
            let scale_y = config.max_height as f32 / height as f32;
            let scale = scale_x.min(scale_y);
            let target_width = (width as f32 * scale).round().max(1.0) as u32;
            let target_height = (height as f32 * scale).round().max(1.0) as u32;
            if config.verbose {
                println!(
                    "Downscaling frame {frame_id} to {}x{} (scale {:.2})",
                    target_width, target_height, scale
                );
            }
            let resized = image::imageops::resize(
                &rgb_image,
                target_width,
                target_height,
                FilterType::Triangle,
            );
            (target_width, target_height, resized.into_raw())
        } else {
            (width, height, rgb_image.into_raw())
        };

    let mut jpeg_bytes = Vec::new();
    {
        let mut cursor = Cursor::new(&mut jpeg_bytes);
        let mut encoder = JpegEncoder::new_with_quality(&mut cursor, config.jpeg_quality);
        encoder
            .encode(
                &frame_pixels,
                frame_width,
                frame_height,
                ColorType::Rgb8.into(),
            )
            .map_err(|err| format!("JPEG encode failed: {err}"))?;
    }

    Ok(FrameData {
        bytes: jpeg_bytes,
        width: frame_width,
        height: frame_height,
    })
}

async fn send_frame(
    connection: &quinn::Connection,
    frame: &FrameData,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut stream = connection.open_uni().await?;
    let frame_len = frame.bytes.len() as u32;
    stream.write_all(&frame_len.to_be_bytes()).await?;
    stream.write_all(&frame.bytes).await?;
    stream.finish().await?;
    Ok(())
}

fn make_client_config() -> ClientConfig {
    struct SkipServerVerification;

    impl ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::Certificate,
            _intermediates: &[rustls::Certificate],
            _server_name: &ServerName,
            _scts: &mut dyn Iterator<Item = &[u8]>,
            _ocsp: &[u8],
            _now: SystemTime,
        ) -> Result<ServerCertVerified, RustlsError> {
            Ok(ServerCertVerified::assertion())
        }
    }

    let crypto = RustlsClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    ClientConfig::new(Arc::new(crypto))
}
