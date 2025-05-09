use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use cpal::{
    SampleFormat, SampleRate, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::{self, Receiver as TokioReceiver}; // Renamed to avoid clash
use tracing::{debug, error, info, warn};
// Use std::sync::mpsc for signaling the std::thread
use std::sync::mpsc as StdMpsc;

use super::AsyncAudioInput;

const SAMPLES_PER_CHUNK_TO_SEND: usize = 1600;

pub struct MicAudioInput {
    sample_rate: u32,
    channels: u16,
    stop_signal_sender: Option<StdMpsc::Sender<()>>, // To signal the std::thread
    thread_join_handle: Option<std::thread::JoinHandle<()>>,
}

impl MicAudioInput {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        if channels != 1 {
            warn!(
                "MicAudioInput configured for {} channels, but 1 (mono) is strongly recommended.",
                channels
            );
        }
        Self {
            sample_rate,
            channels,
            stop_signal_sender: None,
            thread_join_handle: None,
        }
    }
}

impl Drop for MicAudioInput {
    fn drop(&mut self) {
        if self.thread_join_handle.is_some() {
            info!("Dropping MicAudioInput, ensuring cpal thread is stopped.");
            if let Some(sender) = self.stop_signal_sender.take() {
                let _ = sender.send(()); // Signal the thread to stop
            }
            if let Some(handle) = self.thread_join_handle.take() {
                if let Err(e) = handle.join() {
                    error!("Error joining cpal mic input thread on drop: {:?}", e);
                } else {
                    info!("CPAL mic input thread joined successfully on drop.");
                }
            }
        }
    }
}

#[async_trait]
impl AsyncAudioInput for MicAudioInput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }

    async fn start_stream(&mut self) -> Result<TokioReceiver<Vec<i16>>> {
        // Return TokioReceiver
        if self.thread_join_handle.is_some() {
            return Err(anyhow!("Microphone audio stream already started"));
        }

        let (tx_audio_chunks_tokio, rx_audio_chunks_tokio) = mpsc::channel::<Vec<i16>>(100);

        let config_sample_rate = self.sample_rate;
        let config_channels = self.channels;

        let (stop_tx, stop_rx_std): (StdMpsc::Sender<()>, StdMpsc::Receiver<()>) =
            StdMpsc::channel();
        self.stop_signal_sender = Some(stop_tx);

        let join_handle = std::thread::Builder::new()
            .name("cpal-mic-input-thread".into())
            .spawn(move || {
                let host = cpal::default_host();
                let device = match host.default_input_device() {
                    Some(d) => d,
                    None => {
                        error!("[cpal-mic-thread] No default input device.");
                        return;
                    }
                };
                info!(
                    "[cpal-mic-thread] Using input device: {}",
                    device.name().unwrap_or_else(|_| "Unknown".to_string())
                );

                let get_configs = || device.supported_input_configs();
                let supported_config_desc =
                    find_supported_config_generic(get_configs, config_sample_rate, config_channels);

                let supported_config_desc = match supported_config_desc {
                    Ok(sc) => sc,
                    Err(e) => {
                        error!("[cpal-mic-thread] No supported input config: {}", e);
                        return;
                    }
                };

                let final_config: StreamConfig = supported_config_desc.config();
                let sample_format = supported_config_desc.sample_format();
                info!(
                    "[cpal-mic-thread] Selected input config: {:?}, Format: {:?}",
                    final_config, sample_format
                );

                let err_fn = |err| error!("[cpal-mic-thread] Audio input stream error: {}", err);

                let callback_should_stop = Arc::new(AtomicBool::new(false)); // For the cpal callback

                let stream_tx_tokio = tx_audio_chunks_tokio;
                let mut pcm_buffer = Vec::with_capacity(SAMPLES_PER_CHUNK_TO_SEND);

                let callback_stop_clone = callback_should_stop.clone();
                let stream_result = match sample_format {
                    SampleFormat::I16 => device.build_input_stream(
                        &final_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            if callback_stop_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            pcm_buffer.extend_from_slice(data);
                            while pcm_buffer.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                let chunk: Vec<i16> =
                                    pcm_buffer.drain(..SAMPLES_PER_CHUNK_TO_SEND).collect();
                                if stream_tx_tokio.try_send(chunk).is_err() {
                                    // Don't log excessively in callback, can cause issues.
                                    // Consider setting a flag for the outer thread to log.
                                    // debug!("[cpal-mic-callback] Channel closed or full.");
                                    callback_stop_clone.store(true, Ordering::Relaxed); // Stop processing
                                    return;
                                }
                            }
                        },
                        err_fn,
                        None,
                    ),
                    SampleFormat::F32 => device.build_input_stream(
                        &final_config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            if callback_stop_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            let i16_samples: Vec<i16> =
                                data.iter().map(|&s| (s * i16::MAX as f32) as i16).collect();
                            pcm_buffer.extend_from_slice(&i16_samples);
                            while pcm_buffer.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                let chunk: Vec<i16> =
                                    pcm_buffer.drain(..SAMPLES_PER_CHUNK_TO_SEND).collect();
                                if stream_tx_tokio.try_send(chunk).is_err() {
                                    callback_stop_clone.store(true, Ordering::Relaxed);
                                    return;
                                }
                            }
                        },
                        err_fn,
                        None,
                    ),
                    _ => {
                        error!(
                            "[cpal-mic-thread] Unsupported sample format: {:?}",
                            sample_format
                        );
                        return;
                    }
                };

                let stream = match stream_result {
                    Ok(s) => s,
                    Err(e) => {
                        error!("[cpal-mic-thread] Failed to build input stream: {}", e);
                        return;
                    }
                };

                if let Err(e) = stream.play() {
                    error!("[cpal-mic-thread] Failed to play stream: {}", e);
                    return;
                }
                info!("[cpal-mic-thread] Microphone stream playing.");

                // Wait for the stop signal from the main app
                match stop_rx_std.recv() {
                    Ok(()) => info!("[cpal-mic-thread] Stop signal received."),
                    Err(_) => info!("[cpal-mic-thread] Stop signal channel disconnected."),
                }

                info!("[cpal-mic-thread] Stopping microphone stream...");
                callback_should_stop.store(true, Ordering::Relaxed);
                // stream.pause().ok(); // Pause before drop
                drop(stream); // Ensure stream is dropped and resources released by cpal
                info!("[cpal-mic-thread] Microphone thread finished.");
            })
            .context("Failed to spawn cpal mic input thread")?;

        self.thread_join_handle = Some(join_handle);
        Ok(rx_audio_chunks_tokio)
    }

    async fn stop_stream(&mut self) -> Result<()> {
        if let Some(sender) = self.stop_signal_sender.take() {
            if sender.send(()).is_ok() {
                info!("Sent stop signal to cpal mic input thread.");
            } else {
                info!("CPAL mic input thread already stopped or sender disconnected.");
            }
        }
        if let Some(handle) = self.thread_join_handle.take() {
            info!("Waiting for cpal mic input thread to join...");
            match tokio::task::spawn_blocking(move || handle.join()).await {
                Ok(Ok(())) => info!("CPAL mic input thread joined successfully."),
                Ok(Err(e)) => error!("CPAL mic input thread panicked: {:?}", e),
                Err(join_err) => error!("Failed to join CPAL mic input thread: {}", join_err),
            }
        }
        Ok(())
    }
}

// Generic function to find supported config, from the example you provided
pub fn find_supported_config_generic<F, I>(
    mut configs_iterator_fn: F,
    target_sample_rate: u32,
    target_channels: u16,
) -> Result<cpal::SupportedStreamConfig, anyhow::Error>
where
    F: FnMut() -> Result<I, cpal::SupportedStreamConfigsError>,
    I: Iterator<Item = cpal::SupportedStreamConfigRange>,
{
    let mut best_config: Option<cpal::SupportedStreamConfig> = None;
    let mut min_rate_diff = u32::MAX;

    // Iterate through all supported configurations
    for config_range in configs_iterator_fn()? {
        if config_range.channels() != target_channels {
            continue;
        }
        // Prefer i16, but could fall back to f32 if needed.
        // The example logic prioritizes i16.
        if config_range.sample_format() != SampleFormat::I16 {
            continue;
        }

        let current_min_rate = config_range.min_sample_rate().0;
        let current_max_rate = config_range.max_sample_rate().0;

        let rate_to_check =
            if target_sample_rate >= current_min_rate && target_sample_rate <= current_max_rate {
                target_sample_rate
            } else if target_sample_rate < current_min_rate {
                current_min_rate
            } else {
                // target_sample_rate > current_max_rate
                current_max_rate
            };

        let rate_diff = (rate_to_check as i32 - target_sample_rate as i32).abs() as u32;

        if best_config.is_none() || rate_diff < min_rate_diff {
            min_rate_diff = rate_diff;
            best_config = Some(config_range.with_sample_rate(SampleRate(rate_to_check)));
        }

        // If an exact match or the closest possible within a range is found.
        if rate_diff == 0
            && (target_sample_rate >= current_min_rate && target_sample_rate <= current_max_rate)
        {
            break; // Perfect match
        }
    }

    best_config.ok_or_else(|| {
        anyhow::anyhow!(
            "No suitable i16 config found for ~{}Hz {}ch",
            target_sample_rate,
            target_channels
        )
    })
}
