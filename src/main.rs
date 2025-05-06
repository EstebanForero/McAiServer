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
const CHANNELS: u32 = 1; // Assuming mono audio based on the problem description
// and typical handling of raw PCM. If stereo, this needs to be 2
// and LAME configuration/data handling might need adjustment
// (e.g. using lame_encode_buffer_interleaved).
const SECONDS_PER_FILE: usize = 10; // Save MP3 file every 10 seconds

// Calculate the number of i16 samples needed for one file
const SAMPLES_PER_FILE: usize = (SAMPLE_RATE as usize) * SECONDS_PER_FILE * (CHANNELS as usize);

// Buffer size for reading from TCP socket (e.g., 4KB)
const TCP_READ_BUFFER_SIZE: usize = 4096;

/// Encodes PCM samples to MP3 and saves to a timestamped file.
async fn encode_and_save(
    pcm_samples: &mut [i16],
    sample_rate: u32,
    channels: u32,
    file_count: usize, // To make filenames sequential if timestamps are too close
) -> io::Result<()> {
    if pcm_samples.is_empty() {
        println!("No PCM samples to encode.");
        return Ok(());
    }

    println!(
        "Encoding {} PCM samples ({} channels, {} Hz) to MP3...",
        pcm_samples.len(),
        channels,
        sample_rate
    );

    // 1. Initialize LAME
    // Unsafe block is necessary for FFI calls to the C LAME library.
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
    // LAME documentation suggests this formula for max buffer size:
    // For CBR: num_samples_per_channel * (bitrate / 8 / sample_rate) + some_padding
    // A more general one often cited: 1.25 * num_samples_per_channel + 7200
    let num_samples_per_channel = if channels > 0 {
        pcm_samples.len() / (channels as usize)
    } else {
        pcm_samples.len()
    };
    let mp3_buffer_size = (1.25 * num_samples_per_channel as f64 + 7200.0) as usize;
    let mut mp3_buffer = vec![0u8; mp3_buffer_size];

    let encoded_bytes = unsafe {
        // For mono, pcm_right is NULL. nsamples is per channel.
        // If channels were 2 and data interleaved (LRLR...), you'd use lame_encode_buffer_interleaved.
        // Or for non-interleaved stereo, provide separate left and right buffers.
        // Current setup assumes mono.
        lame_sys::lame_encode_buffer(
            lame,
            pcm_samples.as_mut_ptr(), // PCM data for the left channel (or mono channel)
            pcm_samples.as_mut_ptr(), // PCM data for the left channel (or mono channel)
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

    // Collect the initially encoded bytes
    let mut final_mp3_data = mp3_buffer[..encoded_bytes as usize].to_vec();

    // 4. Flush LAME internal buffers to get any remaining MP3 data
    // This call might write more data into mp3_buffer.
    let flush_bytes = unsafe {
        lame_sys::lame_encode_flush(
            lame,
            mp3_buffer.as_mut_ptr(), // Can reuse the buffer
            mp3_buffer_size as std::os::raw::c_int,
        )
    };

    if flush_bytes < 0 {
        unsafe { lame_sys::lame_close(lame) };
        return Err(io::Error::new(io::ErrorKind::Other, "LAME flush failed"));
    }
    // Append flushed bytes to the final MP3 data
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
    let socket = TcpSocket::new_v4()?; // Or new_v6 if needed
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

    let mut pcm_sample_collector = Vec::<i16>::new(); // Collects i16 PCM samples
    let mut partial_byte_buffer = Vec::<u8>::new(); // Stores byte(s) from a previous read that didn't form a full sample
    let mut read_buf = vec![0u8; TCP_READ_BUFFER_SIZE]; // Buffer for socket reads
    let mut file_counter = 0; // Counter for filename uniqueness

    loop {
        match stream.read(&mut read_buf).await {
            Ok(0) => {
                // Stream closed by the peer (EOF)
                println!("Stream ended by peer.");
                if !pcm_sample_collector.is_empty() {
                    println!(
                        "Processing remaining {} PCM samples before exiting...",
                        pcm_sample_collector.len()
                    );
                    file_counter += 1;
                    if let Err(e) = encode_and_save(
                        &mut pcm_sample_collector,
                        SAMPLE_RATE,
                        CHANNELS,
                        file_counter,
                    )
                    .await
                    {
                        eprintln!("Error encoding/saving remaining data: {}", e);
                    }
                } else {
                    println!("No remaining PCM data to process.");
                }
                break; // Exit the loop
            }
            Ok(bytes_read) => {
                // Combine leftover bytes from previous read with newly read data
                let mut current_data_to_process = partial_byte_buffer.clone();
                current_data_to_process.extend_from_slice(&read_buf[..bytes_read]);

                partial_byte_buffer.clear(); // Clear for next potential leftover

                let num_complete_samples = current_data_to_process.len() / 2; // Each i16 sample is 2 bytes

                for i in 0..num_complete_samples {
                    let offset = i * 2;
                    let sample = i16::from_le_bytes([
                        current_data_to_process[offset],
                        current_data_to_process[offset + 1],
                    ]);
                    pcm_sample_collector.push(sample);
                }

                // If there's one byte left over, save it for the next read
                if current_data_to_process.len() % 2 != 0 {
                    partial_byte_buffer
                        .push(current_data_to_process[current_data_to_process.len() - 1]);
                }

                // Check if we have enough samples to create a 10-second MP3 file
                while pcm_sample_collector.len() >= SAMPLES_PER_FILE {
                    file_counter += 1;
                    // Drain the required number of samples from the collector
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
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await; // Small delay
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
                    if let Err(save_err) = encode_and_save(
                        &mut pcm_sample_collector,
                        SAMPLE_RATE,
                        CHANNELS,
                        file_counter,
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
