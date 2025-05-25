use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::{
    io::AsyncReadExt,
    net::TcpStream,
    sync::mpsc::{self, Receiver, Sender},
};
use tracing::{debug, error, info, warn};

use super::AsyncAudioInput;

const TCP_READ_BUFFER_SIZE: usize = 4096;
const SAMPLES_PER_CHUNK_TO_SEND: usize = 1600; // Or whatever chunk size you prefer

// Define an amplification factor
// A value of 1.0 means no change.
// Values > 1.0 amplify.
// Values < 1.0 attenuate.
// Be cautious with large values as they can lead to clipping.
const AMPLIFICATION_FACTOR: f32 = 1.5; // Example: Amplify by 50%

pub struct TcpAudioInput {
    address: String,
    sample_rate: u32,
    channels: u16,
    stop_signal: Arc<Notify>,
    stream_task_handle: Option<tokio::task::JoinHandle<()>>,
    amplification: f32, // Store amplification factor
}

impl TcpAudioInput {
    // Modified constructor to accept an amplification factor
    pub fn new(address: String, sample_rate: u32, channels: u16, amplification: f32) -> Self {
        if channels != 1 {
            warn!(
                "TcpAudioInput created for {} channels, but processing logic assumes mono (1 channel) data.",
                channels
            );
        }
        if amplification < 0.0 {
            warn!(
                "Amplification factor is negative ({}). Using its absolute value.",
                amplification
            );
        }
        info!(
            "TcpAudioInput initialized with amplification factor: {}",
            amplification.abs()
        );
        Self {
            address,
            sample_rate,
            channels,
            stop_signal: Arc::new(Notify::new()),
            stream_task_handle: None,
            amplification: amplification.abs(), // Store the absolute value
        }
    }
}

#[async_trait]
impl AsyncAudioInput for TcpAudioInput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }

    async fn start_stream(&mut self) -> Result<Receiver<Vec<i16>>> {
        if self.stream_task_handle.is_some() {
            return Err(anyhow::anyhow!("TCP audio stream already started"));
        }

        let (tx_audio_chunks, rx_audio_chunks) = mpsc::channel::<Vec<i16>>(100);
        let server_address = self.address.clone();
        let local_stop_signal = self.stop_signal.clone();
        let current_amplification = self.amplification; // Capture current amplification for the task

        info!(
            "Attempting to connect to TCP audio source at {}...",
            server_address
        );
        let tcp_stream = TcpStream::connect(&server_address).await.with_context(|| {
            format!(
                "Failed to connect to TCP audio source at {}",
                server_address
            )
        })?;
        info!(
            "Successfully connected to TCP audio source at {}.",
            server_address
        );

        let task = tokio::spawn(async move {
            process_tcp_to_audio_chunks(
                tcp_stream,
                tx_audio_chunks,
                local_stop_signal,
                current_amplification, // Pass amplification to the processing function
            )
            .await;
        });
        self.stream_task_handle = Some(task);
        Ok(rx_audio_chunks)
    }

    async fn stop_stream(&mut self) -> Result<()> {
        self.stop_signal.notify_waiters();
        if let Some(handle) = self.stream_task_handle.take() {
            info!("Waiting for TCP audio stream processing to shut down...");
            handle
                .await
                .context("TCP audio stream processing task failed")?;
            info!("TCP audio stream processing shut down.");
        }
        Ok(())
    }
}

// Function to amplify and clamp a single sample
fn amplify_sample(sample: i16, factor: f32) -> i16 {
    let amplified = sample as f32 * factor;
    // Clamp to i16 range
    amplified.max(i16::MIN as f32).min(i16::MAX as f32) as i16
}

async fn process_tcp_to_audio_chunks(
    mut stream: TcpStream,
    audio_chunk_sender: Sender<Vec<i16>>,
    stop_signal: Arc<Notify>,
    amplification_factor: f32, // Receive amplification factor
) {
    let mut byte_buffer = Vec::with_capacity(TCP_READ_BUFFER_SIZE * 2); // Can hold up to 2 full TCP reads before processing
    let mut pcm_samples_collected = Vec::with_capacity(SAMPLES_PER_CHUNK_TO_SEND);
    let mut read_buf = vec![0u8; TCP_READ_BUFFER_SIZE];

    info!(
        "Listening for raw PCM (i16 little-endian) audio data from TCP stream. Amplification: x{}",
        amplification_factor
    );

    loop {
        tokio::select! {
            biased; // Prioritize stop signal

            _ = stop_signal.notified() => {
                info!("Stop signal received for TCP stream. Exiting processing loop.");
                break;
            }

            read_result = stream.read(&mut read_buf) => {
                match read_result {
                    Ok(0) => {
                        info!("TCP stream ended by peer (read 0 bytes).");
                        break;
                    }
                    Ok(bytes_read) => {
                        // debug!("Read {} bytes from TCP stream.", bytes_read); // Can be spammy
                        byte_buffer.extend_from_slice(&read_buf[..bytes_read]);

                        // Process bytes in pairs to form i16 samples
                        while byte_buffer.len() >= 2 {
                            let sample_bytes = [byte_buffer[0], byte_buffer[1]];
                            let raw_sample = i16::from_le_bytes(sample_bytes);

                            // **** AMPLIFICATION ****
                            let amplified_sample = amplify_sample(raw_sample, amplification_factor);
                            pcm_samples_collected.push(amplified_sample);

                            byte_buffer.drain(0..2); // Remove processed 2 bytes

                            if pcm_samples_collected.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                if audio_chunk_sender.send(pcm_samples_collected.clone()).await.is_err() {
                                    error!("Failed to send audio chunk: receiver dropped. Exiting TCP loop.");
                                    // Make sure to clear pcm_samples_collected if returning early, or handle ownership.
                                    // Here, returning will drop it.
                                    return;
                                }
                                // debug!("Sent {} amplified samples.", pcm_samples_collected.len()); // Spammy
                                pcm_samples_collected.clear();
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // This typically shouldn't happen with tokio::net::TcpStream's async read
                        // unless it's a non-blocking stream used in a blocking way, which is not the case here.
                        // However, if it did, a small sleep would be appropriate.
                        // For now, we assume standard async behavior where read waits or returns data/error.
                        warn!("TCP stream read would block, this is unexpected in async read. Yielding.");
                        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        continue;
                    }
                    Err(e) => {
                        error!("Failed to read from TCP socket: {}", e);
                        break; // Exit loop on other errors
                    }
                }
            }
        }
    }

    // Send any remaining collected samples
    if !pcm_samples_collected.is_empty() {
        info!(
            "Sending {} remaining amplified samples before TCP stream close.",
            pcm_samples_collected.len()
        );
        if audio_chunk_sender
            .send(pcm_samples_collected) // pcm_samples_collected is already amplified
            .await
            .is_err()
        {
            error!("Failed to send final audio chunk: receiver dropped.");
        }
    }
    info!("TCP audio processing loop finished.");
}
