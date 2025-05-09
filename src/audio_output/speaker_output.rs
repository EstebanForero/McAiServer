// src/audio_output/speaker_output.rs

use anyhow::{Context, Result, anyhow};
use cpal::{
    SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
// Use crossbeam_channel for receiving AI audio
use crossbeam_channel::Receiver as CrossbeamReceiver;
use tracing::{error, info, warn}; // Added warn

// Use the find_supported_config_generic from mic_input
use crate::audio_input::mic_input::find_supported_config_generic;
// Use the constants from gemini_integration
use crate::gemini_integration::{AI_OUTPUT_CHANNELS, AI_OUTPUT_SAMPLE_RATE_HZ};

// This struct will now manage the CPAL stream and its thread.
// It's not an `AsyncAudioOutput` anymore in the sense of returning a sender.
// It's more like a utility that starts playback given a receiver.
pub struct SpeakerPlayback {
    // We don't strictly need to store the stream if we don't control it after creation,
    // but it's good practice for explicit drop/pause if needed.
    _cpal_stream: Option<cpal::Stream>, // Keep stream to ensure it's alive
    // We might not need to store join_handle if we detach or manage thread lifecycle differently
    // but for cleanup, it's good.
    _thread_join_handle: Option<std::thread::JoinHandle<()>>,
}

impl SpeakerPlayback {
    // This function now mirrors `setup_audio_output` from the official example
    pub fn new(ai_audio_receiver: CrossbeamReceiver<Vec<i16>>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("No default output device available"))?;
        info!(
            "[SpeakerPlayback] Using output device: {}",
            device.name().unwrap_or_default()
        );

        // Use AI_OUTPUT_SAMPLE_RATE_HZ and AI_OUTPUT_CHANNELS
        let get_configs = || device.supported_output_configs();
        let supported_config_desc = find_supported_config_generic(
            get_configs,
            AI_OUTPUT_SAMPLE_RATE_HZ, // Use the 24kHz constant
            AI_OUTPUT_CHANNELS,       // Use the 1ch constant
        );

        let supported_config_desc = match supported_config_desc {
            Ok(sc) => sc,
            Err(e) => {
                error!("[SpeakerPlayback] No suitable output config found: {}", e);
                // Fallback or try f32 if i16 fails? For now, error out.
                return Err(e.context("Finding supported speaker config"));
            }
        };

        let final_config: StreamConfig = supported_config_desc.config();
        let final_sample_format = supported_config_desc.sample_format();
        info!(
            "[SpeakerPlayback] Selected output config: {:?}, Format: {:?}",
            final_config, final_sample_format
        );

        let err_fn = |err| error!("[SpeakerPlayback] CPAL audio output error: {}", err);

        // Buffer for the CPAL callback. Size it based on AI_OUTPUT_SAMPLE_RATE_HZ.
        // Official example uses a simple Vec, draining from it. Let's use VecDeque for efficiency.
        let audio_buffer = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let callback_should_stop = Arc::new(AtomicBool::new(false)); // To signal cpal callback

        let stream_buffer_clone = audio_buffer.clone();
        let callback_stop_clone = callback_should_stop.clone();

        let stream = match final_sample_format {
            SampleFormat::I16 => device.build_output_stream(
                &final_config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    if callback_stop_clone.load(Ordering::Relaxed) {
                        for sample in data.iter_mut() {
                            *sample = 0;
                        }
                        return;
                    }
                    let mut buffer = stream_buffer_clone.lock().unwrap();
                    for sample_out in data.iter_mut() {
                        *sample_out = buffer.pop_front().unwrap_or(0); // Play silence on underrun
                    }
                },
                err_fn,
                None,
            )?,
            SampleFormat::F32 => device.build_output_stream(
                &final_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if callback_stop_clone.load(Ordering::Relaxed) {
                        for sample in data.iter_mut() {
                            *sample = 0.0;
                        }
                        return;
                    }
                    let mut buffer = stream_buffer_clone.lock().unwrap();
                    for sample_out in data.iter_mut() {
                        *sample_out = buffer
                            .pop_front()
                            .map_or(0.0, |s_i16| s_i16 as f32 / i16::MAX as f32);
                    }
                },
                err_fn,
                None,
            )?,
            _ => {
                return Err(anyhow!(
                    "Unsupported sample format for speaker output: {:?}",
                    final_sample_format
                ));
            }
        };

        stream.play().context("Failed to play speaker stream")?;
        info!(
            "[SpeakerPlayback] CPAL stream playing with config: {:?}",
            final_config
        );

        // Thread to receive audio from crossbeam channel and feed the Mutex<VecDeque>
        // This thread is simpler than the previous forwarder + cpal thread.
        let playback_thread_audio_buffer = audio_buffer.clone();
        let playback_thread_should_stop = callback_should_stop.clone(); // Reuse stop signal

        let join_handle = std::thread::Builder::new()
            .name("speaker-playback-feed-thread".into())
            .spawn(move || {
                info!("[SpeakerPlaybackFeedThread] Started, waiting for AI audio...");
                for received_samples in ai_audio_receiver {
                    // Iterates until channel is disconnected
                    if playback_thread_should_stop.load(Ordering::Relaxed) {
                        info!("[SpeakerPlaybackFeedThread] Stop signaled, exiting.");
                        break;
                    }
                    if received_samples.is_empty() {
                        continue;
                    }
                    let mut buffer = playback_thread_audio_buffer.lock().unwrap();
                    // Simple buffering strategy: just extend. Could add max size limit.
                    buffer.extend(received_samples);
                }
                info!(
                    "[SpeakerPlaybackFeedThread] Audio channel closed or stop signaled. Exiting."
                );
                // Signal cpal callback to stop filling with silence after buffer empties
                playback_thread_should_stop.store(true, Ordering::Relaxed);
            })
            .context("Failed to spawn speaker playback feed thread")?;

        Ok(Self {
            _cpal_stream: Some(stream),
            _thread_join_handle: Some(join_handle),
        })
    }

    // Add a stop method if needed, or rely on Drop.
    // For explicit cleanup:
    pub fn stop(mut self) -> Result<()> {
        if let Some(jh) = self._thread_join_handle.take() {
            // The thread's stop logic relies on the ai_audio_receiver channel closing.
            // The cpal_stream's callback stop is handled by callback_should_stop.
            info!("[SpeakerPlayback] Stopping... (joining feed thread)");
            jh.join()
                .map_err(|e| anyhow!("Playback feed thread panic: {:?}", e))?;
            info!("[SpeakerPlayback] Playback feed thread joined.");
        }
        if let Some(stream) = self._cpal_stream.take() {
            stream
                .pause()
                .context("Failed to pause cpal output stream on stop")?;
            drop(stream); // Explicitly drop
            info!("[SpeakerPlayback] CPAL stream stopped and dropped.");
        }
        Ok(())
    }
}

// Implement Drop for SpeakerPlayback to ensure thread cleanup
impl Drop for SpeakerPlayback {
    fn drop(&mut self) {
        info!("[SpeakerPlayback] Dropping SpeakerPlayback instance.");
        // Stop logic: The playback_feed_thread will exit when its ai_audio_receiver is dropped (by GeminiAppState).
        // The cpal callback uses an AtomicBool which should also be set if the thread sets it.
        // We might want a more explicit stop signal here for the cpal callback if the stream
        // is not paused/dropped explicitly.
        if let Some(jh) = self._thread_join_handle.take() {
            if !jh.is_finished() {
                info!("[SpeakerPlayback Drop] Waiting for playback feed thread to join...");
                jh.join()
                    .map_err(|e| {
                        warn!(
                            "[SpeakerPlayback Drop] Playback feed thread panic on join: {:?}",
                            e
                        )
                    })
                    .ok();
            }
        }
        if let Some(stream) = self._cpal_stream.take() {
            info!("[SpeakerPlayback Drop] Pausing and dropping CPAL stream.");
            stream.pause().ok(); // Best effort pause
            drop(stream);
        }
    }
}
