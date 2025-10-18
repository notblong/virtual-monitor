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

fn main() {
    // Get receiver IP from args
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: sender <receiver_ip> [max_width] [max_height] [max_fps] [jpeg_quality]");
        return;
    }
    let receiver_ip = &args[1];
    let addr = format!("{}:5000", receiver_ip);
    let max_width = args
        .get(2)
        .and_then(|arg| arg.parse::<u32>().ok())
        .filter(|&val| val > 0)
        .unwrap_or(DEFAULT_MAX_FRAME_WIDTH);
    let max_height = args
        .get(3)
        .and_then(|arg| arg.parse::<u32>().ok())
        .filter(|&val| val > 0)
        .unwrap_or(DEFAULT_MAX_FRAME_HEIGHT);
    let max_fps = args
        .get(4)
        .and_then(|arg| arg.parse::<f32>().ok())
        .filter(|&val| val > 0.0)
        .unwrap_or(DEFAULT_MAX_FPS);
    let jpeg_quality = args
        .get(5)
        .and_then(|arg| arg.parse::<u8>().ok())
        .filter(|&val| val > 0 && val <= 100)
        .unwrap_or(DEFAULT_JPEG_QUALITY);
    let frame_interval = Duration::from_secs_f32(1.0 / max_fps);
    let verbose = env::var("VM_VERBOSE").map(|v| v != "0").unwrap_or(false);

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
