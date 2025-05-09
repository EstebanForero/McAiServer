use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
};
use tracing::{debug, error, info};

use super::AsyncAudioOutput;

pub struct UdpAudioOutput {
    target_address: String,
    socket: Option<Arc<UdpSocket>>,
    stop_signal: Arc<Notify>,
    stream_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl UdpAudioOutput {
    pub fn new(target_address: String) -> Self {
        Self {
            target_address,
            socket: None,
            stop_signal: Arc::new(Notify::new()),
            stream_task_handle: None,
        }
    }
}

#[async_trait]
impl AsyncAudioOutput for UdpAudioOutput {
    async fn start_stream(
        &mut self,
        _sample_rate: u32,
        _channels: u16,
    ) -> Result<Sender<Vec<i16>>> {
        if self.stream_task_handle.is_some() {
            return Err(anyhow!("UDP audio output stream already started"));
        }

        let local_addr = "0.0.0.0:0";
        let socket = UdpSocket::bind(local_addr)
            .await
            .with_context(|| format!("Failed to bind UDP socket to {}", local_addr))?;
        info!(
            "UDP socket bound locally. Ready to send to {}",
            self.target_address
        );

        socket
            .connect(&self.target_address)
            .await
            .with_context(|| {
                format!(
                    "Failed to connect UDP socket to target {}",
                    self.target_address
                )
            })?;
        info!("UDP socket connected to target {}", self.target_address);

        self.socket = Some(Arc::new(socket));
        let socket_clone = self.socket.clone().unwrap();

        let (tx_audio_out, mut rx_audio_out): (Sender<Vec<i16>>, Receiver<Vec<i16>>) =
            mpsc::channel(100);
        let local_stop_signal = self.stop_signal.clone();

        let task = tokio::spawn(async move {
            info!("UDP audio output worker started. Waiting for audio chunks...");
            loop {
                tokio::select! {
                    _ = local_stop_signal.notified() => {
                        info!("Stop signal received for UDP output. Exiting worker.");
                        break;
                    }
                    audio_chunk_option = rx_audio_out.recv() => {
                        match audio_chunk_option {
                            Some(pcm_samples) => {
                                if pcm_samples.is_empty() {
                                    continue;
                                }
                                let mut byte_buffer = Vec::with_capacity(pcm_samples.len() * 2);
                                for sample in pcm_samples {
                                    byte_buffer.extend_from_slice(&sample.to_le_bytes());
                                }

                                match socket_clone.send(&byte_buffer).await {
                                    Ok(bytes_sent) => {
                                        debug!("Sent {} bytes ({} samples) via UDP.", bytes_sent, byte_buffer.len() / 2);
                                    }
                                    Err(e) => {
                                        error!("Failed to send audio data via UDP: {}", e);
                                    }
                                }
                            }
                            None => {
                                info!("UDP audio output channel closed. Exiting worker.");
                                break;
                            }
                        }
                    }
                }
            }
            info!("UDP audio output worker finished.");
        });

        self.stream_task_handle = Some(task);
        Ok(tx_audio_out)
    }

    async fn stop_stream(&mut self) -> Result<()> {
        self.stop_signal.notify_waiters();
        if let Some(handle) = self.stream_task_handle.take() {
            info!("Waiting for UDP audio output stream processing to shut down...");
            handle
                .await
                .context("UDP audio output stream processing task failed")?;
            info!("UDP audio output stream processing shut down.");
        }
        if let Some(socket) = self.socket.take() {
            drop(socket);
        }
        Ok(())
    }
}
