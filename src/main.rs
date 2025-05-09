use chrono::Local;
use std::io;
use tokio::{
    fs::File,
    io::AsyncReadExt,
    io::AsyncWriteExt,
    net::{TcpSocket, TcpStream},
};

// Constants for audio properties
const SAMPLE_RATE: u32 = 16000; // 16kHz
const CHANNELS: u32 = 1; // Assuming mono audio
const SECONDS_PER_FILE: usize = 10; // Save MP3 file every 10 seconds

// Calculate the number of i16 samples needed for one file
const SAMPLES_PER_FILE: usize = (SAMPLE_RATE as usize) * SECONDS_PER_FILE * (CHANNELS as usize);

// Buffer size for reading from TCP socket (e.g., 4KB)
const TCP_READ_BUFFER_SIZE: usize = 4096;

// --- NEW CONSTANT FOR AMPLIFICATION ---
/// Amplification factor.
/// 1.0 = no change.
/// > 1.0 = amplify (e.g., 1.5 for +50% volume, 2.0 for double volume).
/// < 1.0 = attenuate (e.g., 0.5 for half volume).
/// Note: Values too high can lead to clipping if the original audio is already loud.
const AMPLIFICATION_FACTOR: f32 = 200.0; // Amplify by 50%

/// Encodes PCM samples to MP3 and saves to a timestamped file.
async fn encode_and_save(
    pcm_samples: &mut [i16], // Still &mut because lame_encode_buffer takes *mut
    sample_rate: u32,
    channels: u32,
    file_count: usize, // To make filenames sequential if timestamps are too close
    amplification_factor: f32, // Pass the factor
) -> io::Result<()> {
    if pcm_samples.is_empty() {
        println!("No PCM samples to encode.");
        return Ok(());
    }

    println!(
        "Encoding {} PCM samples ({} channels, {} Hz, amplification: {}x) to MP3...",
        pcm_samples.len(),
        channels,
        sample_rate,
        amplification_factor
    );

    // 1. Initialize LAME
    let lame = unsafe { lame_sys::lame_init() };
    if lame.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to initialize LAME encoder",
        ));
    }

    // 2. Set LAME parameters
    unsafe {
        lame_sys::lame_set_in_samplerate(lame, sample_rate as std::os::raw::c_int);
        lame_sys::lame_set_num_channels(lame, channels as std::os::raw::c_int);

        // --- APPLY AMPLIFICATION/SCALING ---
        // lame_set_scale scales the input PCM data before encoding.
        // Values > 1.0 amplify.
        // This is often preferred over manual scaling as LAME handles it internally.
        if amplification_factor != 1.0 {
            // Only set if not default
            // The LAME API uses a float for scaling.
            // For mono, lame_set_scale is sufficient. For stereo, you could use
            // lame_set_scale_left and lame_set_scale_right if you wanted different scaling per channel.
            lame_sys::lame_set_scale(lame, amplification_factor as std::os::raw::c_float);
            println!("LAME input scaling set to: {}", amplification_factor);
        }

        // Configure for CBR (Constant Bit Rate)
        lame_sys::lame_set_VBR(lame, lame_sys::vbr_default); // Turn off VBR
        lame_sys::lame_set_brate(lame, 128); // Set CBR bitrate to 128 kbps

        // Set encoding quality (0=best/slowest, 9=worst/fastest). 2 is high quality.
        lame_sys::lame_set_quality(lame, 2);

        if lame_sys::lame_init_params(lame) < 0 {
            lame_sys::lame_close(lame); // Clean up on error
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to initialize LAME parameters",
            ));
        }
    }

    // 3. Prepare MP3 output buffer
    let num_samples_per_channel = if channels > 0 {
        pcm_samples.len() / (channels as usize)
    } else {
        pcm_samples.len()
    };
    let mp3_buffer_size = (1.25 * num_samples_per_channel as f64 + 7200.0) as usize;
    let mut mp3_buffer = vec![0u8; mp3_buffer_size];

    let encoded_bytes = unsafe {
        lame_sys::lame_encode_buffer(
            lame,
            pcm_samples.as_mut_ptr(),
            pcm_samples.as_mut_ptr(), // For mono, left and right can be the same pointer
            num_samples_per_channel as std::os::raw::c_int,
            mp3_buffer.as_mut_ptr(),
            mp3_buffer_size as std::os::raw::c_int,
        )
    };

    if encoded_bytes < 0 {
        unsafe { lame_sys::lame_close(lame) };
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("LAME encoding failed with code: {}", encoded_bytes),
        ));
    }

    let mut final_mp3_data = mp3_buffer[..encoded_bytes as usize].to_vec();

    // 4. Flush LAME internal buffers
    let flush_bytes = unsafe {
        lame_sys::lame_encode_flush(
            lame,
            mp3_buffer.as_mut_ptr(),
            mp3_buffer_size as std::os::raw::c_int,
        )
    };

    if flush_bytes < 0 {
        unsafe { lame_sys::lame_close(lame) };
        return Err(io::Error::new(io::ErrorKind::Other, "LAME flush failed"));
    }
    final_mp3_data.extend_from_slice(&mp3_buffer[..flush_bytes as usize]);

    // 5. Close LAME encoder
    unsafe {
        lame_sys::lame_close(lame);
    }

    // 6. Save MP3 data to a file
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("audio_{}_{:03}.mp3", timestamp, file_count);

    let mut file = File::create(&filename).await?;
    file.write_all(&final_mp3_data).await?;
    println!("Saved MP3 to {} ({} bytes)", filename, final_mp3_data.len());

    Ok(())
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr_str = "172.20.10.2:8080"; // The IP and port to connect to
    let addr = addr_str.parse().expect("Failed to parse address");

    println!("Attempting to connect to {}...", addr_str);
    let socket = TcpSocket::new_v4()?;
    let mut stream: TcpStream = match socket.connect(addr).await {
        Ok(s) => {
            println!("Successfully connected to {}", addr_str);
            s
        }
        Err(e) => {
            eprintln!("Failed to connect to {}: {}", addr_str, e);
            return Err(e);
        }
    };
    println!("Listening for raw PCM audio data...");

    let mut pcm_sample_collector = Vec::<i16>::new();
    let mut partial_byte_buffer = Vec::<u8>::new();
    let mut read_buf = vec![0u8; TCP_READ_BUFFER_SIZE];
    let mut file_counter = 0;

    loop {
        match stream.read(&mut read_buf).await {
            Ok(0) => {
                println!("Stream ended by peer.");
                if !pcm_sample_collector.is_empty() {
                    println!(
                        "Processing remaining {} PCM samples before exiting...",
                        pcm_sample_collector.len()
                    );
                    file_counter += 1;
                    // Ensure pcm_sample_collector is mutable for encode_and_save
                    let mut samples_to_save = pcm_sample_collector.clone(); // Clone if you need collector later, or just pass it
                    pcm_sample_collector.clear(); // If you don't need it after this

                    if let Err(e) = encode_and_save(
                        &mut samples_to_save, // Pass the mutable Vec
                        SAMPLE_RATE,
                        CHANNELS,
                        file_counter,
                        AMPLIFICATION_FACTOR, // Pass factor
                    )
                    .await
                    {
                        eprintln!("Error encoding/saving remaining data: {}", e);
                    }
                } else {
                    println!("No remaining PCM data to process.");
                }
                break;
            }
            Ok(bytes_read) => {
                let mut current_data_to_process = partial_byte_buffer.clone();
                current_data_to_process.extend_from_slice(&read_buf[..bytes_read]);
                partial_byte_buffer.clear();

                let num_complete_samples = current_data_to_process.len() / 2;
                for i in 0..num_complete_samples {
                    let offset = i * 2;
                    let sample = i16::from_le_bytes([
                        current_data_to_process[offset],
                        current_data_to_process[offset + 1],
                    ]);
                    pcm_sample_collector.push(sample);
                }

                if current_data_to_process.len() % 2 != 0 {
                    partial_byte_buffer
                        .push(current_data_to_process[current_data_to_process.len() - 1]);
                }

                while pcm_sample_collector.len() >= SAMPLES_PER_FILE {
                    file_counter += 1;
                    let mut samples_for_this_file: Vec<i16> =
                        pcm_sample_collector.drain(..SAMPLES_PER_FILE).collect();

                    println!(
                        "Collected {} samples for file #{}. Encoding...",
                        samples_for_this_file.len(),
                        file_counter
                    );
                    if let Err(e) = encode_and_save(
                        &mut samples_for_this_file,
                        SAMPLE_RATE,
                        CHANNELS,
                        file_counter,
                        AMPLIFICATION_FACTOR, // Pass factor
                    )
                    .await
                    {
                        eprintln!(
                            "Error during encoding/saving MP3 file #{}: {}",
                            file_counter, e
                        );
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                continue;
            }
            Err(e) => {
                eprintln!("Failed to read from socket: {}", e);
                if !pcm_sample_collector.is_empty() {
                    println!(
                        "Attempting to process {} remaining samples due to read error...",
                        pcm_sample_collector.len()
                    );
                    file_counter += 1;
                    let mut samples_to_save = pcm_sample_collector.clone();
                    pcm_sample_collector.clear();

                    if let Err(save_err) = encode_and_save(
                        &mut samples_to_save,
                        SAMPLE_RATE,
                        CHANNELS,
                        file_counter,
                        AMPLIFICATION_FACTOR, // Pass factor
                    )
                    .await
                    {
                        eprintln!(
                            "Error encoding/saving remaining data after read error: {}",
                            save_err
                        );
                    }
                }
                return Err(e);
            }
        }
    }

    println!("Program finished.");
    Ok(())
}
