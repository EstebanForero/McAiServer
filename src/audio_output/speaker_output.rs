use async_trait::async_trait;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, SampleRate, StreamConfig,
};
use tokio::sync::mpsc::{self, Receiver, Sender}; // Receiver needed for the thread
use anyhow::{Result, Context, anyhow};
use tracing::{info, error, debug}; // Removed unused warn
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use tokio::sync::Notify;
use std::collections::VecDeque;

use super::AsyncAudioOutput;

const SPEAKER_BUFFER_TARGET_MS: usize = 200;

pub struct SpeakerAudioOutput {
    // Signal for the dedicated cpal std::thread to stop
    thread_stop_notifier: Option<Arc<Notify>>,
    // Join handle for the cpal std::thread
    thread_join_handle: Option<std::thread::JoinHandle<()>>,
    // Sender to send audio data TO the cpal std::thread
    audio_data_sender_to_thread: Option<Sender<Vec<i16>>>,
}

impl SpeakerAudioOutput {
    pub fn new() -> Self {
        Self {
            thread_stop_notifier: None,
            thread_join_handle: None,
            audio_data_sender_to_thread: None,
        }
    }
}

impl Drop for SpeakerAudioOutput {
    fn drop(&mut self) {
        if self.thread_join_handle.is_some() {
            info!("Dropping SpeakerAudioOutput, ensuring cpal thread is stopped.");
            // Dropping audio_data_sender_to_thread signals the thread's recv loop.
            self.audio_data_sender_to_thread.take();
            if let Some(notifier) = self.thread_stop_notifier.take() {
                notifier.notify_one();
            }
            if let Some(handle) = self.thread_join_handle.take() {
                if let Err(e) = handle.join() {
                    error!("Error joining cpal speaker output thread on drop: {:?}", e);
                }
            }
        }
    }
}

#[async_trait]
impl AsyncAudioOutput for SpeakerAudioOutput {
    async fn start_stream(&mut self, sample_rate: u32, channels: u16) -> Result<Sender<Vec<i16>>> {
        if self.thread_join_handle.is_some() {
            return Err(anyhow!("Speaker audio output stream already started"));
        }

        // This channel sends audio data from main app to the cpal thread
        let (tx_to_cpal_thread, mut rx_from_main_app): (Sender<Vec<i16>>, Receiver<Vec<i16>>) = mpsc::channel(100);
        self.audio_data_sender_to_thread = Some(tx_to_cpal_thread.clone()); // Keep a clone to return

        let thread_stop_notifier_arc = Arc::new(Notify::new());
        self.thread_stop_notifier = Some(thread_stop_notifier_arc.clone());

        let join_handle = std::thread::Builder::new()
            .name("cpal-speaker-output-thread".into())
            .spawn(move || {
                let host = cpal::default_host();
                let device = match host.default_output_device() {
                    Some(d) => d,
                    None => { error!("[cpal-speaker-thread] No default output device."); return; }
                };
                info!("[cpal-speaker-thread] Using output device: {}", device.name().unwrap_or_default());

                // ... (config detection logic as in mic_input, for output configs) ...
                 let mut supported_configs_range = match device.supported_output_configs() {
                    Ok(r) => r, Err(e) => {error!("[cpal-speaker-thread] Err query output conf: {}", e); return; }
                };
                 let supported_config_desc = supported_configs_range.find(/*... find i16 or f32 ...*/).unwrap(); // placeholder
                let final_config: StreamConfig = supported_config_desc.with_sample_rate(SampleRate(sample_rate)).config();
                let final_sample_format = supported_config_desc.sample_format();
                info!("[cpal-speaker-thread] Selected output config: {:?}, Format: {:?}", final_config, final_sample_format);


                let err_fn = |err| error!("[cpal-speaker-thread] Audio output error: {}", err);

                let samples_in_buffer = (sample_rate as usize * SPEAKER_BUFFER_TARGET_MS / 1000) * channels as usize;
                let cpal_thread_audio_buffer = Arc::new(Mutex::new(VecDeque::<i16>::with_capacity(samples_in_buffer * 2)));
                let callback_should_stop = Arc::new(AtomicBool::new(false));

                let stream_buffer_clone = cpal_thread_audio_buffer.clone();
                let callback_stop_clone = callback_should_stop.clone();

                let stream_result = match final_sample_format {
                    SampleFormat::I16 => device.build_output_stream(
                        &final_config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            if callback_stop_clone.load(Ordering::Relaxed) {
                                for sample in data.iter_mut() { *sample = 0; } return;
                            }
                            let mut buffer = stream_buffer_clone.lock().unwrap();
                            for sample_out in data.iter_mut() {
                                *sample_out = buffer.pop_front().unwrap_or(0);
                            }
                        },
                        err_fn, None),
                    SampleFormat::F32 => device.build_output_stream(
                        &final_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if callback_stop_clone.load(Ordering::Relaxed) {
                                for sample in data.iter_mut() { *sample = 0.0; } return;
                            }
                            let mut buffer = stream_buffer_clone.lock().unwrap();
                            for sample_out in data.iter_mut() {
                                *sample_out = buffer.pop_front().map_or(0.0, |s| s as f32 / i16::MAX as f32);
                            }
                        },
                        err_fn, None),
                    _ => { error!("[cpal-speaker-thread] Unsupported output format."); return; }
                };
                
                let stream = match stream_result {
                    Ok(s) => s, Err(e) => { error!("[cpal-speaker-thread] Failed to build output stream: {}", e); return; }
                };
                if let Err(e) = stream.play() { error!("[cpal-speaker-thread] Failed to play output: {}", e); return; }
                info!("[cpal-speaker-thread] Speaker stream playing.");

                // Loop to receive audio from main app and fill buffer
                loop {
                    tokio::runtime::Handle::current().block_on(async { // Need a way to run async recv in sync thread
                        tokio::select! {
                            biased;
                            _ = thread_stop_notifier_arc.notified() => {
                                // This branch will be taken when notify_one() is called
                                debug!("[cpal-speaker-thread] Stop notifier was triggered.");
                                // No action needed here, loop termination is handled by audio_chunk_option being None or outer logic
                            }
                            audio_chunk_option = rx_from_main_app.recv() => {
                                match audio_chunk_option {
                                    Some(pcm_samples) => {
                                        let mut buffer = cpal_thread_audio_buffer.lock().unwrap();
                                        if buffer.len() + pcm_samples.len() > buffer.capacity() {
                                            // Simple overflow handling: drop new samples or oldest, here dropping oldest.
                                            let space_needed = (buffer.len() + pcm_samples.len()) - buffer.capacity();
                                            buffer.drain(..space_needed.min(buffer.len()));
                                        }
                                        buffer.extend(pcm_samples);
                                    }
                                    None => { // Sender (from main app) was dropped
                                        info!("[cpal-speaker-thread] Audio data channel closed. Will play out buffer.");
                                        // Set flag to break outer loop after this select! block
                                        // No direct break here, allow one last check of notifier.
                                    }
                                }
                            }
                        }
                    }); // end block_on

                    // Check if the main sender is dropped or if notifier was hit explicitly for stopping
                    if rx_from_main_app.is_closed() || thread_stop_notifier_arc.is_notified() { // is_notified is not a public method. Use AtomicBool
                        break;
                    }
                }
                
                // Wait for buffer to play out slightly or stop signal
                info!("[cpal-speaker-thread] Main audio source ended. Waiting for buffer to play out...");
                while cpal_thread_audio_buffer.lock().unwrap().len() > (sample_rate as usize / 10) { // Play until ~100ms left
                    if thread_stop_notifier_arc.is_notified() { break; } // Fast exit if stop is signaled
                     std::thread::sleep(std::time::Duration::from_millis(50));
                }

                info!("[cpal-speaker-thread] Stopping speaker stream playback.");
                callback_should_stop.store(true, Ordering::Relaxed);
                // Stream is dropped when thread scope ends.
            })
            .context("Failed to spawn cpal speaker output thread")?;
        
        self.thread_join_handle = Some(join_handle);
        Ok(tx_to_cpal_thread) // Return the sender for the main app to send audio TO this cpal thread
    }

    async fn stop_stream(&mut self) -> Result<()> {
        // Dropping the sender will cause the recv() loop in the cpal thread to get None
        self.audio_data_sender_to_thread.take(); 
        
        if let Some(notifier) = self.thread_stop_notifier.take() {
            notifier.notify_one();
            info!("Signaled cpal speaker output thread to stop.");
        }
        if let Some(handle) = self.thread_join_handle.take() {
            info!("Waiting for cpal speaker output thread to join...");
            match tokio::task::spawn_blocking(move || handle.join()).await {
                Ok(Ok(())) => info!("CPAL speaker output thread joined successfully."),
                Ok(Err(e)) => error!("CPAL speaker output thread panicked: {:?}", e),
                Err(join_err) => error!("Failed to join CPAL speaker output thread: {}", join_err),
            }
        }
        Ok(())
    }
}
