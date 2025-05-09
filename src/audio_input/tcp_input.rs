use anyhow::{Context, Result};
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
const SAMPLES_PER_CHUNK_TO_SEND: usize = 1600;

pub struct TcpAudioInput {
    address: String,
    sample_rate: u32,
    channels: u16,
    stop_signal: Arc<Notify>,
    stream_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TcpAudioInput {
    pub fn new(address: String, sample_rate: u32, channels: u16) -> Self {
        if channels != 1 {
            warn!(
                "TcpAudioInput created for {} channels, but processing logic assumes mono (1 channel) data from the socket for Gemini.",
                channels
            );
        }
        Self {
            address,
            sample_rate,
            channels,
            stop_signal: Arc::new(Notify::new()),
            stream_task_handle: None,
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
            process_tcp_to_audio_chunks(tcp_stream, tx_audio_chunks, local_stop_signal).await;
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

async fn process_tcp_to_audio_chunks(
    mut stream: TcpStream,
    audio_chunk_sender: Sender<Vec<i16>>,
    stop_signal: Arc<Notify>,
) {
    let mut byte_buffer = Vec::with_capacity(TCP_READ_BUFFER_SIZE * 2);
    let mut pcm_samples_collected = Vec::with_capacity(SAMPLES_PER_CHUNK_TO_SEND);
    let mut read_buf = vec![0u8; TCP_READ_BUFFER_SIZE];

    info!("Listening for raw PCM (i16 little-endian) audio data from TCP stream...");

    loop {
        tokio::select! {
            biased;

            _ = stop_signal.notified() => {
                info!("Stop signal received for TCP stream. Exiting processing loop.");
                break;
            }

            read_result = stream.read(&mut read_buf) => {
                match read_result {
                    Ok(0) => {
                        info!("TCP stream ended by peer.");
                        break;
                    }
                    Ok(bytes_read) => {
                        debug!("Read {} bytes from TCP stream.", bytes_read);
                        byte_buffer.extend_from_slice(&read_buf[..bytes_read]);

                        while byte_buffer.len() >= 2 {
                            let sample_bytes = [byte_buffer[0], byte_buffer[1]];
                            let sample = i16::from_le_bytes(sample_bytes);
                            pcm_samples_collected.push(sample);
                            byte_buffer.drain(0..2);

                            if pcm_samples_collected.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                if audio_chunk_sender.send(pcm_samples_collected.clone()).await.is_err() {
                                    error!("Failed to send audio chunk: receiver dropped. Exiting TCP loop.");
                                    return;
                                }
                                pcm_samples_collected.clear();
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                        continue;
                    }
                    Err(e) => {
                        error!("Failed to read from TCP socket: {}", e);
                        break;
                    }
                }
            }
        }
    }

    if !pcm_samples_collected.is_empty() {
        info!(
            "Sending {} remaining samples before TCP stream close.",
            pcm_samples_collected.len()
        );
        if audio_chunk_sender
            .send(pcm_samples_collected)
            .await
            .is_err()
        {
            error!("Failed to send final audio chunk: receiver dropped.");
        }
    }
    info!("TCP audio processing loop finished.");
}
