use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::sync::Arc;
use tokio::io::AsyncWriteExt; // For writing to TcpStream
use tokio::net::TcpStream; // For TCP connection
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use super::AsyncAudioOutput; // Keep if you want to maintain the trait structure
use tokio::sync::mpsc::Sender as TokioSender; // For trait, if used

// No explicit chunking needed at this level for TCP, as TCP is a stream.
// However, we might still process audio in logical chunks from the CrossbeamReceiver.

pub struct TcpAudioOutput {
    target_address: String, // e.g., "ESP32_IP:SPEAKER_TCP_PORT"
    stop_signal: Arc<Notify>,
    stream_task_handle: Option<tokio::task::JoinHandle<()>>,
    // We might store the TcpStream if we want to manage it outside the task,
    // but usually, the task manages its own connection.
}

impl TcpAudioOutput {
    pub fn new(target_address: String) -> Self {
        Self {
            target_address,
            stop_signal: Arc::new(Notify::new()),
            stream_task_handle: None,
        }
    }

    // Similar to UdpAudioOutput, this starts the worker task
    pub async fn start_with_crossbeam_receiver(
        &mut self,
        audio_rx: CrossbeamReceiver<Vec<i16>>, // Audio comes from this channel
        _sample_rate: u32,                     // For logging or future use
        _channels: u16,                        // For logging or future use
    ) -> Result<()> {
        if self.stream_task_handle.is_some() {
            return Err(anyhow!(
                "TCP audio output (from crossbeam) stream already started"
            ));
        }

        let local_stop_signal = self.stop_signal.clone();
        let target_addr_clone = self.target_address.clone();

        // The task will own the TcpStream connection lifecycle
        let task = tokio::spawn(async move {
            debug!(
                "[TCP_XBeam_Worker->{}] Started. Attempting to connect...",
                target_addr_clone
            );

            // Connection loop in case of disconnects (optional, can be simpler to just fail)
            let mut stream: Option<TcpStream> = None;
            let mut connection_attempts = 0;

            // Try to connect to the ESP32 TCP server
            // For simplicity, this example tries once. For robustness, add retries.
            match TcpStream::connect(&target_addr_clone).await {
                Ok(s) => {
                    info!(
                        "[TCP_XBeam_Worker->{}] Successfully connected to ESP32.",
                        target_addr_clone
                    );
                    stream = Some(s);
                }
                Err(e) => {
                    error!(
                        "[TCP_XBeam_Worker->{}] Failed to connect to ESP32: {}. Worker will exit.",
                        target_addr_clone, e
                    );
                    // If connection fails, the task exits, and no audio will be sent.
                    // You might want a retry mechanism here.
                    return;
                }
            }

            // Unwrap is safe here due to the return above on error
            let mut active_stream = stream.unwrap();

            info!(
                "[TCP_XBeam_Worker->{}] Waiting for audio from CrossbeamReceiver...",
                target_addr_clone
            );
            loop {
                let recv_result: Result<Option<Vec<i16>>, tokio::task::JoinError> =
                    tokio::task::spawn_blocking({
                        let audio_rx_clone = audio_rx.clone();
                        move || audio_rx_clone.recv().ok()
                    })
                    .await;

                tokio::select! {
                    biased;
                    _ = local_stop_signal.notified() => {
                        info!("[TCP_XBeam_Worker->{}] Stop signal received. Closing connection and exiting.", target_addr_clone);
                        // active_stream.shutdown().await.ok(); // Gracefully close write side
                        break;
                    }
                    audio_option = async { recv_result } => {
                        match audio_option {
                            Ok(Some(pcm_samples)) => {
                                if pcm_samples.is_empty() {
                                    debug!("[TCP_XBeam_Worker->{}] Empty PCM samples from Crossbeam, skipping.", target_addr_clone);
                                    continue;
                                }
                                debug!("[TCP_XBeam_Worker->{}] Received {} samples from Crossbeam. Preparing to send over TCP.", target_addr_clone, pcm_samples.len());

                                let mut byte_buffer = Vec::with_capacity(pcm_samples.len() * 2);
                                for sample in pcm_samples {
                                    byte_buffer.extend_from_slice(&sample.to_le_bytes());
                                }

                                // Send the byte buffer over TCP
                                // TCP is a stream, so no explicit chunking like UDP is needed here.
                                // `write_all` ensures all data is sent.
                                debug!("[TCP_XBeam_Worker->{}] Sending {} bytes ({} samples) via TCP.", target_addr_clone, byte_buffer.len(), byte_buffer.len() / 2);
                                match active_stream.write_all(&byte_buffer).await {
                                    Ok(_) => {
                                        debug!("[TCP_XBeam_Worker->{}] Sent {} bytes via TCP.", target_addr_clone, byte_buffer.len());
                                    }
                                    Err(e) => {
                                        error!("[TCP_XBeam_Worker->{}] Failed to send TCP data: {}. ESP32 likely disconnected. Exiting.", target_addr_clone, e);
                                        // If send fails, assume connection is dead and exit the loop.
                                        break;
                                    }
                                }
                            }
                            Ok(None) => {
                                info!("[TCP_XBeam_Worker->{}] Crossbeam audio source channel closed. Closing TCP connection and exiting.", target_addr_clone);
                                // active_stream.shutdown().await.ok(); // Gracefully close write side
                                break;
                            }
                            Err(e) => {
                                error!("[TCP_XBeam_Worker->{}] spawn_blocking error for Crossbeam recv: {}. Exiting.", target_addr_clone, e);
                                break;
                            }
                        }
                    }
                }
            }
            // Attempt to gracefully shut down the write side of the stream.
            // This sends a FIN packet.
            match active_stream.shutdown().await {
                Ok(_) => info!(
                    "[TCP_XBeam_Worker->{}] TCP stream write side shut down gracefully.",
                    target_addr_clone
                ),
                Err(e) => warn!(
                    "[TCP_XBeam_Worker->{}] Error shutting down TCP stream: {}",
                    target_addr_clone, e
                ),
            }
            info!("[TCP_XBeam_Worker->{}] Finished.", target_addr_clone);
        });

        self.stream_task_handle = Some(task);
        Ok(())
    }

    pub async fn stop_stream_processing(&mut self) -> Result<()> {
        self.stop_signal.notify_waiters(); // Signal the worker task to stop
        if let Some(handle) = self.stream_task_handle.take() {
            info!("[TCP_XBeam] Waiting for TCP stream (from Crossbeam) processing to shut down...");
            handle
                .await
                .context("[TCP_XBeam] TCP stream (from Crossbeam) processing task failed")?;
            info!("[TCP_XBeam] TCP stream (from Crossbeam) processing shut down.");
        }
        Ok(())
    }
}
