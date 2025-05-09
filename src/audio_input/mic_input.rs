use async_trait::async_trait;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, SampleRate, StreamConfig,
};
use tokio::sync::mpsc::{self, Receiver};
use anyhow::{Result, Context, anyhow};
use tracing::{info, error, warn, debug};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::sync::Notify; // For waking the std::thread if it's designed to sleep/poll

use super::AsyncAudioInput;

const SAMPLES_PER_CHUNK_TO_SEND: usize = 1600;

pub struct MicAudioInput {
    sample_rate: u32,
    channels: u16,
    // Signal for the dedicated cpal std::thread to stop
    thread_stop_notifier: Option<Arc<Notify>>,
    // Join handle for the cpal std::thread
    thread_join_handle: Option<std::thread::JoinHandle<()>>,
}

impl MicAudioInput {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        if channels != 1 {
            warn!("MicAudioInput configured for {} channels, but 1 (mono) is strongly recommended.", channels);
        }
        Self {
            sample_rate,
            channels,
            thread_stop_notifier: None,
            thread_join_handle: None,
        }
    }
}

impl Drop for MicAudioInput {
    fn drop(&mut self) {
        if self.thread_join_handle.is_some() {
            info!("Dropping MicAudioInput, ensuring cpal thread is stopped.");
            if let Some(notifier) = self.thread_stop_notifier.take() {
                notifier.notify_one();
            }
            if let Some(handle) = self.thread_join_handle.take() {
                if let Err(e) = handle.join() {
                    error!("Error joining cpal mic input thread on drop: {:?}", e);
                }
            }
        }
    }
}

#[async_trait]
impl AsyncAudioInput for MicAudioInput {
    fn sample_rate(&self) -> u32 { self.sample_rate }
    fn channels(&self) -> u16 { self.channels }

    async fn start_stream(&mut self) -> Result<Receiver<Vec<i16>>> {
        if self.thread_join_handle.is_some() {
            return Err(anyhow!("Microphone audio stream already started"));
        }

        let (tx_audio_chunks, rx_audio_chunks) = mpsc::channel::<Vec<i16>>(100);
        
        let config_sample_rate = self.sample_rate;
        let config_channels = self.channels;

        let thread_stop_notifier_arc = Arc::new(Notify::new());
        self.thread_stop_notifier = Some(thread_stop_notifier_arc.clone());

        let join_handle = std::thread::Builder::new()
            .name("cpal-mic-input-thread".into())
            .spawn(move || {
                let host = cpal::default_host();
                let device = match host.default_input_device() {
                    Some(d) => d,
                    None => {
                        error!("[cpal-mic-thread] No default input device available.");
                        return;
                    }
                };
                info!("[cpal-mic-thread] Using input device: {}", device.name().unwrap_or_else(|_| "Unknown".to_string()));

                let mut supported_configs_range = match device.supported_input_configs() {
                    Ok(r) => r,
                    Err(e) => {
                        error!("[cpal-mic-thread] Error querying input configs: {}", e);
                        return;
                    }
                };

                let supported_config_desc = supported_configs_range
                    .find(|config| {
                        config.sample_format() == SampleFormat::I16
                            && config.channels() == config_channels
                            && config.min_sample_rate() <= SampleRate(config_sample_rate)
                            && config.max_sample_rate() >= SampleRate(config_sample_rate)
                    })
                    .or_else(|| {
                        device.supported_input_configs().ok().and_then(|mut cfgs| cfgs.find(|config| {
                            config.sample_format() == SampleFormat::F32
                                && config.channels() == config_channels
                                && config.min_sample_rate() <= SampleRate(config_sample_rate)
                                && config.max_sample_rate() >= SampleRate(config_sample_rate)
                        }))
                    })
                    .ok_or_else(|| {
                        error!("[cpal-mic-thread] No supported input config found for {} Hz, {} ch.", config_sample_rate, config_channels);
                        anyhow!("No supported input config") // Error type for thread
                    });

                let supported_config_desc = match supported_config_desc {
                    Ok(sc) => sc,
                    Err(_) => return,
                };
                
                let final_config: StreamConfig = supported_config_desc.with_sample_rate(SampleRate(config_sample_rate)).config();
                let sample_format = supported_config_desc.sample_format();
                info!("[cpal-mic-thread] Selected input config: {:?}, Format: {:?}", final_config, sample_format);

                let err_fn = |err| error!("[cpal-mic-thread] Audio input stream error: {}", err);
                
                let callback_should_stop = Arc::new(AtomicBool::new(false));
                
                let stream_tx = tx_audio_chunks; // Move sender into thread
                let mut pcm_buffer = Vec::with_capacity(SAMPLES_PER_CHUNK_TO_SEND);

                let callback_stop_clone = callback_should_stop.clone();
                let stream_result = match sample_format {
                    SampleFormat::I16 => device.build_input_stream(
                        &final_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            if callback_stop_clone.load(Ordering::Relaxed) { return; }
                            pcm_buffer.extend_from_slice(data);
                            while pcm_buffer.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                let chunk: Vec<i16> = pcm_buffer.drain(..SAMPLES_PER_CHUNK_TO_SEND).collect();
                                if stream_tx.try_send(chunk).is_err() {
                                    debug!("[cpal-mic-callback] Channel closed, stopping data send.");
                                    callback_stop_clone.store(true, Ordering::Relaxed);
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
                            if callback_stop_clone.load(Ordering::Relaxed) { return; }
                            let i16_samples: Vec<i16> = data.iter().map(|&s| (s * i16::MAX as f32) as i16).collect();
                            pcm_buffer.extend_from_slice(&i16_samples);
                            while pcm_buffer.len() >= SAMPLES_PER_CHUNK_TO_SEND {
                                let chunk: Vec<i16> = pcm_buffer.drain(..SAMPLES_PER_CHUNK_TO_SEND).collect();
                                if stream_tx.try_send(chunk).is_err() {
                                    debug!("[cpal-mic-callback-f32] Channel closed, stopping data send.");
                                    callback_stop_clone.store(true, Ordering::Relaxed);
                                    return;
                                }
                            }
                        },
                        err_fn,
                        None,
                    ),
                    _ => {
                        error!("[cpal-mic-thread] Unsupported sample format: {:?}", sample_format);
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

                // Park the thread until notified to stop. The `stream` object is kept alive.
                thread_stop_notifier_arc.notified().blocking_wait();
                
                info!("[cpal-mic-thread] Stop signal received. Stopping microphone stream...");
                callback_should_stop.store(true, Ordering::Relaxed); // Signal callback
                // Stream is dropped automatically when this thread's scope ends.
            })
            .context("Failed to spawn cpal mic input thread")?;

        self.thread_join_handle = Some(join_handle);
        Ok(rx_audio_chunks)
    }

    async fn stop_stream(&mut self) -> Result<()> {
        if let Some(notifier) = self.thread_stop_notifier.take() {
            notifier.notify_one();
            info!("Signaled cpal mic input thread to stop.");
        }
        if let Some(handle) = self.thread_join_handle.take() {
            info!("Waiting for cpal mic input thread to join...");
            // Spawn_blocking to avoid blocking the async executor with join
            match tokio::task::spawn_blocking(move || handle.join()).await {
                Ok(Ok(())) => info!("CPAL mic input thread joined successfully."),
                Ok(Err(e)) => error!("CPAL mic input thread panicked: {:?}", e),
                Err(join_err) => error!("Failed to join CPAL mic input thread: {}", join_err),
            }
        }
        Ok(())
    }
}
