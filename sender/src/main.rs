use image::codecs::jpeg::JpegEncoder;
use image::ColorType;
use screenshots::Screen;
use std::io::Cursor;
use std::{
    env,
    net::UdpSocket,
    thread,
    time::{Duration, Instant},
};

const CHUNK_SIZE: usize = 1_200; // keep under typical MTU to avoid fragmentation

fn main() {
    // Get receiver IP from args
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: sender <receiver_ip>");
        return;
    }
    let receiver_ip = &args[1];
    let addr = format!("{}:5000", receiver_ip);

    // Create UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind failed");
    socket
        .set_nonblocking(false)
        .expect("cannot set blocking mode");

    // Capture main screen
    let screen = Screen::from_point(0, 0).expect("no screen found");
    println!(
        "Streaming display {}x{} to {}",
        screen.display_info.width, screen.display_info.height, addr
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

            // Encode to JPEG in-memory
            let mut jpeg_bytes = Vec::new();
            {
                let mut cursor = Cursor::new(&mut jpeg_bytes);
                let mut encoder = JpegEncoder::new_with_quality(&mut cursor, 70);
                encoder
                    .encode(&rgb_buffer, width, height, ColorType::Rgb8.into())
                    .expect("JPEG encode failed");
            }

            // Split into chunks
            let total_chunks = ((jpeg_bytes.len() + CHUNK_SIZE - 1) / CHUNK_SIZE) as u16;
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

        // Control FPS (~10–15 fps)
        let elapsed = start.elapsed();
        if elapsed < Duration::from_millis(100) {
            thread::sleep(Duration::from_millis(100) - elapsed);
        }
    }
}
