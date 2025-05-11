use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
// We are removing the tokio::mpsc imports for the data channel
use crossbeam_channel::Receiver as CrossbeamReceiver; // << NEW: To receive audio
use tracing::{debug, error, info, warn};

// We might not perfectly fit this trait anymore if we change how UdpAudioOutput gets its data.
// Let's keep it for now and see, or decide to remove it for UdpAudioOutput.
use super::AsyncAudioOutput;
use tokio::sync::mpsc::Sender as TokioSender; // Still needed for trait, even if unused

pub struct UdpAudioOutput {
    target_address: String,
    // No internal tokio mpsc Sender/Receiver for audio data anymore.
    // The UdpSocket will be created and managed by the processing task.
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

    // New method to start the stream with a CrossbeamReceiver
    pub async fn start_with_crossbeam_receiver(
        &mut self,
        audio_rx: CrossbeamReceiver<Vec<i16>>, // Audio comes from this channel
        // sample_rate and channels can be passed if needed for logging or future use
        _sample_rate: u32,
        _channels: u16,
    ) -> Result<()> {
        if self.stream_task_handle.is_some() {
            return Err(anyhow!(
                "UDP audio output (from crossbeam) stream already started"
            ));
        }

        let local_addr = "0.0.0.0:0"; // Bind to any available local port
        let socket = UdpSocket::bind(local_addr)
            .await
            .with_context(|| format!("[UDP_XBeam] Failed to bind UDP socket to {}", local_addr))?;
        info!(
            "[UDP_XBeam] Socket bound locally. Target: {}",
            self.target_address
        );

        // Connect to the target address. This sets the default destination for `send`.
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

        let socket_arc = Arc::new(socket); // Share the socket with the task
        let local_stop_signal = self.stop_signal.clone();
        let target_addr_clone = self.target_address.clone(); // For logging within the task

        let task = tokio::spawn(async move {
            info!(
                "[UDP_XBeam_Worker->{}] Started. Waiting for audio from CrossbeamReceiver...",
                target_addr_clone
            );
            loop {
                // We need to wait for either a stop signal or new audio.
                // crossbeam_channel::recv() is blocking.
                let recv_result: Result<Option<Vec<i16>>, tokio::task::JoinError> =
                    tokio::task::spawn_blocking({
                        let audio_rx_clone = audio_rx.clone(); // Clone for the blocking task
                        move || audio_rx_clone.recv().ok() // .ok() converts RecvError (channel closed) to None
                    })
                    .await;

                tokio::select! {
                    biased; // Prioritize stop signal

                    _ = local_stop_signal.notified() => {
                        info!("[UDP_XBeam_Worker->{}] Stop signal received. Exiting.", target_addr_clone);
                        break;
                    }

                    // Process the outcome of the blocking receive operation
                    audio_option = async { recv_result } => {
                        match audio_option {
                            Ok(Some(pcm_samples)) => { // Successfully received audio
                                if pcm_samples.is_empty() {
                                    debug!("[UDP_XBeam_Worker->{}] Received empty PCM samples from Crossbeam, skipping.", target_addr_clone);
                                    continue;
                                }
                                debug!("[UDP_XBeam_Worker->{}] Received {} samples from Crossbeam. Preparing to send.", target_addr_clone, pcm_samples.len());

                                let mut byte_buffer = Vec::with_capacity(pcm_samples.len() * 2);
                                for sample in pcm_samples { // Iterate by value is fine if pcm_samples is owned
                                    byte_buffer.extend_from_slice(&sample.to_le_bytes());
                                }

                                info!("[UDP_XBeam_Worker->{}] Sending {} bytes ({} samples) via UDP.", target_addr_clone, byte_buffer.len(), byte_buffer.len() / 2);
                                match socket_arc.send(&byte_buffer).await {
                                    Ok(bytes_sent) => {
                                        debug!("[UDP_XBeam_Worker->{}] Sent {} bytes via UDP.", target_addr_clone, bytes_sent);
                                    }
                                    Err(e) => {
                                        error!("[UDP_XBeam_Worker->{}] Failed to send UDP data: {}",target_addr_clone, e);
                                        // Potentially break here if send errors are critical
                                    }
                                }
                            }
                            Ok(None) => { // Crossbeam channel was closed
                                info!("[UDP_XBeam_Worker->{}] Crossbeam audio source channel closed. Exiting.", target_addr_clone);
                                break;
                            }
                            Err(e) => { // Error from spawn_blocking itself (e.g., panic in the blocking code)
                                error!("[UDP_XBeam_Worker->{}] Error in spawn_blocking for Crossbeam recv: {}. Exiting.", target_addr_clone, e);
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

    // This method is for stopping the worker task.
    pub async fn stop_stream_processing(&mut self) -> Result<()> {
        self.stop_signal.notify_waiters();
        if let Some(handle) = self.stream_task_handle.take() {
            info!("[UDP_XBeam] Waiting for UDP stream (from Crossbeam) processing to shut down...");
            handle
                .await
                .context("[UDP_XBeam] UDP stream (from Crossbeam) processing task failed")?;
            info!("[UDP_XBeam] UDP stream (from Crossbeam) processing shut down.");
        }
        Ok(())
    }
}

// The AsyncAudioOutput trait implementation needs careful consideration.
// If `start_stream` is meant to return a Sender for THIS module to RECEIVE data,
// and we've now changed it to receive from a CrossbeamReceiver given at start-up,
// the original trait contract is hard to fulfill directly.
#[async_trait]
impl AsyncAudioOutput for UdpAudioOutput {
    async fn start_stream(
        &mut self,
        _sample_rate: u32,
        _channels: u16,
    ) -> Result<TokioSender<Vec<i16>>> {
        // This method is problematic now.
        // For this specific use case where audio is fed via start_with_crossbeam_receiver,
        // this method shouldn't ideally be called or should return an error/dummy.
        warn!(
            "[UDP_XBeam] `start_stream` called on UdpAudioOutput configured for Crossbeam. This is likely not intended. Use `start_with_crossbeam_receiver`."
        );
        // Return a dummy channel or an error to satisfy the trait but indicate misuse.
        // let (tx, _rx) = tokio::sync::mpsc::channel(1); // Dummy channel
        // Ok(tx)
        Err(anyhow!(
            "UdpAudioOutput (Crossbeam version) should be started with `start_with_crossbeam_receiver`, not `start_stream` from trait for data input."
        ))
    }

    async fn stop_stream(&mut self) -> Result<()> {
        // Delegate to the new stop method
        self.stop_stream_processing().await
    }
}
