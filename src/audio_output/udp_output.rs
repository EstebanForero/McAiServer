use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use super::AsyncAudioOutput;
use tokio::sync::mpsc::Sender as TokioSender;

// Define a reasonable max payload size for UDP packets
const MAX_UDP_PAYLOAD_BYTES: usize = 1400; // Approx 700 i16 samples
// const MAX_SAMPLES_PER_UDP_PACKET: usize = MAX_UDP_PAYLOAD_BYTES / 2;

pub struct UdpAudioOutput {
    target_address: String,
    stop_signal: Arc<Notify>,
    stream_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl UdpAudioOutput {
    pub fn new(target_address: String) -> Self {
        Self {
            target_address,
            stop_signal: Arc::new(Notify::new()),
            stream_task_handle: None,
        }
    }

    pub async fn start_with_crossbeam_receiver(
        &mut self,
        audio_rx: CrossbeamReceiver<Vec<i16>>,
        _sample_rate: u32,
        _channels: u16,
    ) -> Result<()> {
        if self.stream_task_handle.is_some() {
            return Err(anyhow!(
                "UDP audio output (from crossbeam) stream already started"
            ));
        }

        let local_addr = "0.0.0.0:0";
        let socket = UdpSocket::bind(local_addr)
            .await
            .with_context(|| format!("[UDP_XBeam] Failed to bind UDP socket to {}", local_addr))?;
        info!(
            "[UDP_XBeam] Socket bound locally. Target: {}",
            self.target_address
        );

        socket
            .connect(&self.target_address)
            .await
            .with_context(|| {
                format!(
                    "[UDP_XBeam] Failed to connect UDP socket to target {}",
                    self.target_address
                )
            })?;
        info!(
            "[UDP_XBeam] Socket connected to target {}",
            self.target_address
        );

        let socket_arc = Arc::new(socket);
        let local_stop_signal = self.stop_signal.clone();
        let target_addr_clone = self.target_address.clone();

        let task = tokio::spawn(async move {
            info!(
                "[UDP_XBeam_Worker->{}] Started. Max UDP payload: {} bytes. Waiting for audio...",
                target_addr_clone, MAX_UDP_PAYLOAD_BYTES
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
                        info!("[UDP_XBeam_Worker->{}] Stop signal. Exiting.", target_addr_clone);
                        break;
                    }
                    audio_option = async { recv_result } => {
                        match audio_option {
                            Ok(Some(pcm_samples)) => {
                                if pcm_samples.is_empty() {
                                    debug!("[UDP_XBeam_Worker->{}] Empty PCM samples, skipping.", target_addr_clone);
                                    continue;
                                }
                                debug!("[UDP_XBeam_Worker->{}] Received {} samples. Chunking for UDP.", target_addr_clone, pcm_samples.len());

                                // Convert Vec<i16> to Vec<u8> (little-endian)
                                let mut full_byte_buffer = Vec::with_capacity(pcm_samples.len() * 2);
                                for sample in pcm_samples {
                                    full_byte_buffer.extend_from_slice(&sample.to_le_bytes());
                                }

                                // Send the byte buffer in chunks
                                for chunk in full_byte_buffer.chunks(MAX_UDP_PAYLOAD_BYTES) {
                                    if chunk.is_empty() { continue; }

                                    // Ensure we send an even number of bytes if samples are i16
                                    // This is usually handled if MAX_UDP_PAYLOAD_BYTES is even and chunks are full,
                                    // but the last chunk might be odd if not careful.
                                    // However, ESP32's i2s_write takes bytes and should handle it.
                                    // For safety, ensure chunk.len() is even.
                                    let bytes_to_send = if chunk.len() % 2 != 0 && chunk.len() > 1 {
                                        warn!("[UDP_XBeam_Worker->{}] Odd byte chunk detected ({} bytes), might truncate last byte for i16 safety. This should ideally not happen if source data is purely i16.", target_addr_clone, chunk.len());
                                        // This case should be rare if pcm_samples always gives complete i16 data.
                                        &chunk[..chunk.len() -1]
                                    } else {
                                        chunk
                                    };

                                    if bytes_to_send.is_empty() { continue; }


                                    info!("[UDP_XBeam_Worker->{}] Sending UDP CHUNK: {} bytes.", target_addr_clone, bytes_to_send.len());
                                    match socket_arc.send(bytes_to_send).await {
                                        Ok(bytes_sent_in_chunk) => {
                                            debug!("[UDP_XBeam_Worker->{}] Sent {} bytes in UDP chunk.", target_addr_clone, bytes_sent_in_chunk);
                                        }
                                        Err(e) => {
                                            error!("[UDP_XBeam_Worker->{}] Failed to send UDP chunk: {}",target_addr_clone, e);
                                            // Decide if we should break the inner loop or continue with next chunk
                                        }
                                    }
                                    // Small delay between sending chunks if needed, though usually not necessary for UDP locally.
                                    // tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                                }
                            }
                            Ok(None) => {
                                info!("[UDP_XBeam_Worker->{}] Crossbeam channel closed. Exiting.", target_addr_clone);
                                break;
                            }
                            Err(e) => {
                                error!("[UDP_XBeam_Worker->{}] spawn_blocking error: {}. Exiting.", target_addr_clone, e);
                                break;
                            }
                        }
                    }
                }
            }
            info!("[UDP_XBeam_Worker->{}] Finished.", target_addr_clone);
        });

        self.stream_task_handle = Some(task);
        Ok(())
    }

    pub async fn stop_stream_processing(&mut self) -> Result<()> {
        self.stop_signal.notify_waiters();
        if let Some(handle) = self.stream_task_handle.take() {
            info!("[UDP_XBeam] Shutting down UDP stream processing...");
            handle.await.context("[UDP_XBeam] Task failed")?;
            info!("[UDP_XBeam] UDP stream processing shut down.");
        }
        Ok(())
    }
}

#[async_trait]
impl AsyncAudioOutput for UdpAudioOutput {
    async fn start_stream(
        &mut self,
        _sample_rate: u32,
        _channels: u16,
    ) -> Result<TokioSender<Vec<i16>>> {
        warn!(
            "[UDP_XBeam] `start_stream` called (likely from trait). Use `start_with_crossbeam_receiver` for direct Crossbeam input."
        );
        Err(anyhow!(
            "UdpAudioOutput (Crossbeam) should use `start_with_crossbeam_receiver`."
        ))
    }

    async fn stop_stream(&mut self) -> Result<()> {
        self.stop_stream_processing().await
    }
}
