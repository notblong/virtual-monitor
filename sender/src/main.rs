use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ColorType, RgbImage};
use screenshots::Screen;
use std::io::Cursor;
use std::{
    env,
    net::UdpSocket,
    thread,
    time::{Duration, Instant},
};

const CHUNK_SIZE: usize = 1_200; // keep under typical MTU to avoid fragmentation
const DEFAULT_MAX_FRAME_WIDTH: u32 = 1_280;
const DEFAULT_MAX_FRAME_HEIGHT: u32 = 720;
const DEFAULT_MAX_FPS: f32 = 30.0;
const DEFAULT_JPEG_QUALITY: u8 = 60;
const DEFAULT_RECEIVER_IP: &str = "127.0.0.1";
const USAGE: &str = "\
Usage: sender [options] [receiver_ip]

Options:
  --receiver <ip>          Receiver IPv4/IPv6 address (default 127.0.0.1 or first positional)
  --max-width <pixels>     Maximum frame width (default 1280)
  --max-height <pixels>    Maximum frame height (default 720)
  --max-fps <fps>          Target frames per second (default 30.0)
  --jpeg-quality <1-100>   JPEG quality percentage (default 60)
  --verbose / -v           Enable verbose logging (can also set VM_VERBOSE=1)
  --no-verbose             Disable verbose logging
  --help                   Show this message

Examples:
  cargo run --release -- --receiver 192.168.1.50 --max-width 1600 --max-fps 45
  cargo run --release -- --jpeg-quality 30
";

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

fn main() {
    let config = match SenderConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) if err.is_empty() => return,
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            return;
        }
    };
    let addr = format!("{}:5000", config.receiver);
    let frame_interval = Duration::from_secs_f32(1.0 / config.max_fps);
    let verbose = config.verbose;
    let max_width = config.max_width;
    let max_height = config.max_height;
    let max_fps = config.max_fps;
    let jpeg_quality = config.jpeg_quality;

    // Create UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind failed");
    socket
        .set_nonblocking(false)
        .expect("cannot set blocking mode");

    // Capture main screen
    let screen = Screen::from_point(0, 0).expect("no screen found");
    println!(
        "Streaming display {}x{} to {} (max frame size {}x{}, max {:.1} fps, JPEG quality {})",
        screen.display_info.width,
        screen.display_info.height,
        addr,
        max_width,
        max_height,
        max_fps,
        jpeg_quality
    );

    let mut frame_id: u32 = 0;

    loop {
        let start = Instant::now();
        if let Ok(capture) = screen.capture() {
            let width = capture.width();
            let height = capture.height();

            // JPEG encoder only supports RGB input, so drop alpha channel from RGBA buffer.
            let rgba = capture.rgba();
            let mut rgb_buffer = Vec::with_capacity((width as usize) * (height as usize) * 3);
            for pixel in rgba.chunks_exact(4) {
                rgb_buffer.push(pixel[0]); // R
                rgb_buffer.push(pixel[1]); // G
                rgb_buffer.push(pixel[2]); // B
            }
            let rgb_image = match RgbImage::from_raw(width, height, rgb_buffer) {
                Some(img) => img,
                None => {
                    eprintln!(
                        "Failed to build RGB image for frame {frame_id} ({}x{})",
                        width, height
                    );
                    continue;
                }
            };

            // Downscale if needed to stay within max dimensions.
            let (frame_width, frame_height, frame_pixels) =
                if width > max_width || height > max_height {
                    let scale_x = max_width as f32 / width as f32;
                    let scale_y = max_height as f32 / height as f32;
                    let scale = scale_x.min(scale_y);
                    let target_width = (width as f32 * scale).round().max(1.0) as u32;
                    let target_height = (height as f32 * scale).round().max(1.0) as u32;
                    if verbose {
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

            // Encode to JPEG in-memory
            let mut jpeg_bytes = Vec::new();
            {
                let mut cursor = Cursor::new(&mut jpeg_bytes);
                let mut encoder = JpegEncoder::new_with_quality(&mut cursor, jpeg_quality);
                encoder
                    .encode(
                        &frame_pixels,
                        frame_width,
                        frame_height,
                        ColorType::Rgb8.into(),
                    )
                    .expect("JPEG encode failed");
            }

            // Split into chunks
            let total_chunks = ((jpeg_bytes.len() + CHUNK_SIZE - 1) / CHUNK_SIZE) as u16;
            if verbose {
                println!(
                    "Prepared frame {frame_id}: {}x{} -> {} bytes across {total_chunks} chunks",
                    frame_width,
                    frame_height,
                    jpeg_bytes.len()
                );
            }
            for i in 0..total_chunks {
                let start_i = i as usize * CHUNK_SIZE;
                let end_i = std::cmp::min(start_i + CHUNK_SIZE, jpeg_bytes.len());
                let chunk = &jpeg_bytes[start_i..end_i];

                // Build packet: [frame_id (4 bytes)] [chunk_index (2 bytes)] [total_chunks (2 bytes)] [data...]
                let mut packet = Vec::with_capacity(8 + chunk.len());
                packet.extend_from_slice(&frame_id.to_be_bytes());
                packet.extend_from_slice(&(i as u16).to_be_bytes());
                packet.extend_from_slice(&total_chunks.to_be_bytes());
                packet.extend_from_slice(chunk);

                socket.send_to(&packet, &addr).ok();
            }

            frame_id = frame_id.wrapping_add(1);
        }

        // Throttle to configured FPS.
        let elapsed = start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }
    }
}
