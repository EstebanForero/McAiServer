use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender as CrossbeamSender;
use std::{fs, sync::Arc, time::Duration};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{Level, debug, error, info, warn};

mod audio_input;
mod audio_output;
mod config;
mod openai_integration;

use audio_input::{
    AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput as Esp32MicTcpInput,
};
use audio_output::{
    speaker_output::SpeakerPlayback, tcp_output::TcpAudioOutput as Esp32SpeakerTcpOutput,
};

use config::Config;
use openai_integration::{
    AI_OUTPUT_CHANNELS as AI_OUTPUT_CHANNELS_CONST, AI_OUTPUT_SAMPLE_RATE_HZ, OpenAiAppState,
    create_openai_client,
}; // Renamed for clarity

fn get_initial_prompt() -> Option<String> {
    fs::read_to_string("prompt.txt").ok()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO) // Set to DEBUG for more verbose TCP logs if needed
        .with_target(true)
        .init();

    info!("🚀 Starting OpenAI Voice Assistant (ESP32 TCP Edition)...");
    let app_config = Config::from_env().expect("🚨 Failed to load configuration from .env");

    // Determine the actual input sample rate based on source
    // For OpenAI, we always need to send 24kHz.
    // If source is ESP32, it's already 24kHz.
    // If source is local MIC, app_config.audio_sample_rate should be 24kHz, or resampling is needed.
    // We are now assuming app_config.audio_sample_rate is the target rate for the source.
    let input_audio_source_sample_rate = app_config.audio_sample_rate;
    if input_audio_source_sample_rate != 24000 {
        warn!(
            "Input audio source sample rate is {} Hz. OpenAI's 'pcm16' requires 24000 Hz. Ensure your source matches or implement resampling.",
            input_audio_source_sample_rate
        );
    }

    let mut audio_input_source: Box<dyn AsyncAudioInput> =
        match app_config.audio_source.to_uppercase().as_str() {
            "TCP" => {
                info!(
                    "🎤 Using TCP audio input from ESP32 at rate: {} Hz",
                    input_audio_source_sample_rate
                );
                Box::new(Esp32MicTcpInput::new(
                    app_config
                        .tcp_server_address // ESP32 Mic Server
                        .clone()
                        .expect("TCP_SERVER_ADDRESS for ESP32 mic input missing"),
                    input_audio_source_sample_rate, // ESP32 sends at this rate (should be 24kHz)
                    app_config.audio_channels,
                ))
            }
            "MIC" => {
                info!(
                    "🎤 Using local MIC audio input at rate: {} Hz",
                    input_audio_source_sample_rate
                );
                Box::new(MicAudioInput::new(
                    input_audio_source_sample_rate, // Local mic configured rate (should be 24kHz for direct OpenAI use)
                    app_config.audio_channels,
                ))
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Invalid AUDIO_SOURCE in .env. Use TCP or MIC."
                ));
            }
        };

    let (ai_audio_tx_crossbeam, ai_audio_rx_crossbeam) =
        crossbeam_channel::bounded::<Vec<i16>>(100);

    let openai_playback_sender_for_state: Option<CrossbeamSender<Vec<i16>>>;

    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    let mut esp32_speaker_tcp_output_module: Option<Esp32SpeakerTcpOutput> = None;

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!(
                "🔊 Using local Speaker audio output (via crossbeam channel). Output at {} Hz.",
                AI_OUTPUT_SAMPLE_RATE_HZ
            );
            _speaker_playback_manager = Some(
                SpeakerPlayback::new(ai_audio_rx_crossbeam)
                    .context("Failed to initialize SpeakerPlayback")?,
            );
            openai_playback_sender_for_state = Some(ai_audio_tx_crossbeam);
        }
        "TCP_SPEAKER" => {
            let target_addr = app_config
                .speaker_tcp_output_address
                .clone()
                .expect("SPEAKER_TCP_TARGET_ADDRESS for ESP32 Speaker TCP output missing");
            info!(
                "🔊 Using TCP audio output to ESP32 Speaker at: {}. Output audio will be {} Hz.",
                target_addr, AI_OUTPUT_SAMPLE_RATE_HZ
            );
            let mut tcp_module = Esp32SpeakerTcpOutput::new(target_addr);
            tcp_module
                .start_with_crossbeam_receiver(
                    ai_audio_rx_crossbeam,    // Audio from OpenAI (via state) will go here
                    AI_OUTPUT_SAMPLE_RATE_HZ, // OpenAI provides audio at this rate
                    AI_OUTPUT_CHANNELS_CONST, // OpenAI provides mono
                )
                .await
                .context("Failed to start ESP32 Speaker TCP output with Crossbeam receiver")?;
            esp32_speaker_tcp_output_module = Some(tcp_module);
            openai_playback_sender_for_state = Some(ai_audio_tx_crossbeam.clone());
        }
        "NONE" | "" => {
            info!("🔇 Audio output is disabled.");
            openai_playback_sender_for_state = None;
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid AUDIO_OUTPUT_TYPE. Use SPEAKER, TCP_SPEAKER, or NONE."
            ));
        }
    }

    let initial_prompt = get_initial_prompt();

    let mut ai_client = create_openai_client(
        &app_config,
        initial_prompt,
        openai_playback_sender_for_state,
    )
    .await?;
    let ai_app_state_clone: Arc<OpenAiAppState> = ai_client.state();

    let mut audio_chunk_receiver_tokio: TokioReceiver<Vec<i16>> =
        audio_input_source.start_stream().await?;
    info!("🎧 Audio input stream started. Listening for audio chunks...");
    info!("🗣️  Assistant is now listening. Press Ctrl+C to exit.");

    let mut last_audio_sent_time = tokio::time::Instant::now();
    const VAD_SILENCE_DURATION: Duration = Duration::from_secs(3); // Slightly longer for TCP/network variance

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, initiating shutdown...");
                break;
            }

            audio_chunk_option = audio_chunk_receiver_tokio.recv() => {
                match audio_chunk_option {
                    Some(audio_chunk_vec) => {
                        if !audio_chunk_vec.is_empty() {
                            debug!("➡️ Received audio frame ({} samples from source at {} Hz). Forwarding to OpenAI at 24kHz.",
                                   audio_chunk_vec.len(), input_audio_source_sample_rate);

                            // CRITICAL: Send audio to OpenAI at 24000 Hz, which is what pcm16 requires.
                            // The `input_audio_source_sample_rate` must match this, or resampling is needed.
                            // We are currently assuming `input_audio_source_sample_rate` is already 24000Hz.
                            if let Err(e) = ai_client.send_audio_chunk(
                                &audio_chunk_vec,
                                24000, // Explicitly send as 24kHz to OpenAI
                            ).await {
                                error!("🚨 Failed to send audio to OpenAI: {}.", e);
                                if matches!(e, gemini_live_api::error::GeminiError::SendError | gemini_live_api::error::GeminiError::ConnectionClosed) {
                                    error!("Connection lost. Shutting down.");
                                    break;
                                }
                            }
                            last_audio_sent_time = tokio::time::Instant::now();
                        }
                    }
                    None => {
                        info!("🔇 Audio input stream (mic/tcp_mic) ended. Sending audio_stream_end.");
                        if let Err(e) = ai_client.send_audio_stream_end().await {
                             error!("🚨 Failed to send final audio_stream_end to OpenAI: {}.", e);
                        }
                        break; // Exit main loop if audio source ends
                    }
                }
            }

            _ = tokio::time::sleep_until(last_audio_sent_time + VAD_SILENCE_DURATION), if tokio::time::Instant::now() >= last_audio_sent_time + VAD_SILENCE_DURATION => {
                if !ai_app_state_clone.is_ai_speaking() { // Check if AI is currently speaking
                    info!("[VAD] Silence detected for {}s. Sending audio_stream_end to OpenAI.", VAD_SILENCE_DURATION.as_secs());
                    if let Err(e) = ai_client.send_audio_stream_end().await {
                        error!("🚨 Failed to send audio_stream_end (VAD) to OpenAI: {}.", e);
                         if matches!(e, gemini_live_api::error::GeminiError::SendError | gemini_live_api::error::GeminiError::ConnectionClosed) {
                            error!("Connection lost after VAD. Shutting down.");
                            break;
                        }
                    }
                    // Prevent rapid VAD triggers if AI is about to speak or just finished
                    last_audio_sent_time = tokio::time::Instant::now() + Duration::from_secs(1000); // Effectively disable VAD until new audio comes
                } else {
                    debug!("[VAD] Silence detected, but AI is speaking. Deferring audio_stream_end.");
                    last_audio_sent_time = tokio::time::Instant::now(); // Reset VAD timer to wait for AI to finish
                }
            }


            _ = ai_app_state_clone.turn_complete_signal.notified() => {
                info!("[MainLoop] OpenAI turn complete signaled. Ready for new input.");
                ai_app_state_clone.current_turn_text.lock().unwrap().clear();
                ai_app_state_clone.set_ai_is_speaking(false); // AI finished speaking
                last_audio_sent_time = tokio::time::Instant::now(); // Reset VAD timer
            }
        }
    }

    info!("🔌 Shutting down audio input source (mic/tcp_mic)...");
    if let Err(e) = audio_input_source.stop_stream().await {
        error!("🚨 Error stopping audio input source: {}", e);
    }

    if let Some(mut spm) = _speaker_playback_manager.take() {
        info!("🔌 Shutting down local speaker playback manager...");
        if let Err(e) = spm.stop() {
            error!("🚨 Error stopping local speaker playback: {}", e);
        }
    }

    if let Some(mut tcp_module) = esp32_speaker_tcp_output_module.take() {
        info!("🔌 Shutting down ESP32 Speaker TCP audio output...");
        if let Err(e) = tcp_module.stop_stream_processing().await {
            // Renamed method
            error!("🚨 Error stopping ESP32 Speaker TCP audio output: {}", e);
        }
    }

    info!("🤖 Closing OpenAI client connection...");
    if !ai_client.is_closed() {
        if let Err(e) = ai_client.send_audio_stream_end().await {
            // Ensure final end is sent if loop broke early
            warn!(
                "🚨 Attempted to send final audio_stream_end on shutdown: {}.",
                e
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await; // Give a moment for last messages
        if let Err(e) = ai_client.close().await {
            error!("🚨 Error closing OpenAI client: {}", e);
        }
    }

    info!("👋 OpenAI Voice Assistant shut down gracefully. Goodbye!");
    Ok(())
}
