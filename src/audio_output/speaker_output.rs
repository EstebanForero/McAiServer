use anyhow::{Context, Result, anyhow};
use cpal::{
    SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::Receiver as CrossbeamReceiver;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tracing::{error, info, warn};

use crate::audio_input::mic_input::find_supported_config_generic;
use crate::openai_integration::{AI_OUTPUT_CHANNELS, AI_OUTPUT_SAMPLE_RATE_HZ};

pub struct SpeakerPlayback {
    _cpal_stream: Option<cpal::Stream>,
    _thread_join_handle: Option<std::thread::JoinHandle<()>>,
    _stop_signal: Arc<AtomicBool>,
}

impl SpeakerPlayback {
    pub fn new(ai_audio_receiver: CrossbeamReceiver<Vec<i16>>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("No output device"))?;
        info!("[SP] Using output: {}", device.name().unwrap_or_default());

        let get_configs = || device.supported_output_configs();
        let supported_config_desc = find_supported_config_generic(
            get_configs,
            AI_OUTPUT_SAMPLE_RATE_HZ,
            AI_OUTPUT_CHANNELS,
        )
        .context("Finding speaker config")?;

        let final_config: StreamConfig = supported_config_desc.config();
        let final_sample_format = supported_config_desc.sample_format();
        info!(
            "[SP] Config: {:?}, Format: {:?}",
            final_config, final_sample_format
        );

        let err_fn = |err| error!("[SP_CPAL_CB] Error: {}", err);
        let audio_buffer = Arc::new(Mutex::new(VecDeque::<i16>::new()));

        let master_stop_signal = Arc::new(AtomicBool::new(false));

        let stream_buffer_clone = audio_buffer.clone();
        let cpal_callback_stop_signal = master_stop_signal.clone();

        let stream = match final_sample_format {
            SampleFormat::I16 => device.build_output_stream(
                &final_config,
                move |data: &mut [i16], _| {
                    if cpal_callback_stop_signal.load(Ordering::Relaxed) {
                        data.iter_mut().for_each(|s| *s = 0);
                        return;
                    }
                    let mut buffer = stream_buffer_clone.lock().unwrap();
                    data.iter_mut()
                        .for_each(|s_out| *s_out = buffer.pop_front().unwrap_or(0));
                },
                err_fn,
                None,
            )?,
            SampleFormat::F32 => device.build_output_stream(
                &final_config,
                move |data: &mut [f32], _| {
                    if cpal_callback_stop_signal.load(Ordering::Relaxed) {
                        data.iter_mut().for_each(|s| *s = 0.0);
                        return;
                    }
                    let mut buffer = stream_buffer_clone.lock().unwrap();
                    data.iter_mut().for_each(|s_out| {
                        *s_out = buffer
                            .pop_front()
                            .map_or(0.0, |s| s as f32 / i16::MAX as f32)
                    });
                },
                err_fn,
                None,
            )?,
            _ => {
                return Err(anyhow!(
                    "Unsupported speaker format: {:?}",
                    final_sample_format
                ));
            }
        };
        stream.play().context("Failed to play speaker stream")?;
        info!("[SP] CPAL stream playing.");

        let feed_thread_audio_buffer = audio_buffer.clone();
        let feed_thread_stop_signal = master_stop_signal.clone();

        let join_handle = std::thread::Builder::new()
            .name("speaker-feed-thread".into())
            .spawn(move || {
                info!("[SP_Feed] Started.");
                for received_samples in ai_audio_receiver {
                    if feed_thread_stop_signal.load(Ordering::Relaxed) {
                        info!("[SP_Feed] Stop signaled (master), exiting.");
                        break;
                    }
                    if received_samples.is_empty() {
                        continue;
                    }

                    let mut buffer = feed_thread_audio_buffer.lock().unwrap();
                    if feed_thread_stop_signal.load(Ordering::Relaxed) {
                        info!("[SP_Feed] Stop signaled (master post-lock), exiting.");
                        break;
                    }
                    buffer.extend(received_samples);
                }
                info!("[SP_Feed] Channel closed or stop signal. Exiting.");
                feed_thread_stop_signal.store(true, Ordering::Relaxed);
            })
            .context("Failed to spawn speaker feed thread")?;

        Ok(Self {
            _cpal_stream: Some(stream),
            _thread_join_handle: Some(join_handle),
            _stop_signal: master_stop_signal,
        })
    }

    pub fn stop(&mut self) -> Result<()> {
        // Changed to &mut self
        info!("[SP] stop() called.");
        self._stop_signal.store(true, Ordering::Relaxed);

        if let Some(jh) = self._thread_join_handle.take() {
            info!("[SP] Joining feed thread...");
            jh.join()
                .map_err(|e| anyhow!("Feed thread panic: {:?}", e))?;
            info!("[SP] Feed thread joined.");
        }
        if let Some(stream) = self._cpal_stream.take() {
            info!("[SP] Pausing CPAL stream...");
            stream
                .pause()
                .context("Failed to pause cpal stream on stop")?;
            drop(stream);
            info!("[SP] CPAL stream stopped and dropped.");
        }
        Ok(())
    }
}

impl Drop for SpeakerPlayback {
    fn drop(&mut self) {
        info!("[SP] Dropping SpeakerPlayback.");
        if self._cpal_stream.is_some() || self._thread_join_handle.is_some() {
            self._stop_signal.store(true, Ordering::Relaxed);
            if let Some(jh) = self._thread_join_handle.take() {
                if !jh.is_finished() {
                    info!("[SP Drop] Waiting for feed thread to join...");
                    jh.join()
                        .map_err(|e| warn!("[SP Drop] Feed thread panic: {:?}", e))
                        .ok();
                }
            }
            if let Some(stream) = self._cpal_stream.take() {
                info!("[SP Drop] Pausing CPAL stream.");
                stream.pause().ok();
            }
        }
    }
}
