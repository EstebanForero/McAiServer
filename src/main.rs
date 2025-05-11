use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender as CrossbeamSender;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{Level, debug, error, info, warn};

mod audio_input;
mod audio_output;
mod config;
mod gemini_integration;

use audio_input::{
    AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput as MicTcpInput,
}; // Renamed for clarity
// Use the new TcpAudioOutput for speaker
use audio_output::{speaker_output::SpeakerPlayback, tcp_output::TcpAudioOutput}; // <<< CHANGED
// AsyncAudioOutput trait is still here.
// use audio_output::AsyncAudioOutput; // Not strictly needed if we don't store TcpAudioOutput as Box<dyn...>

use config::Config;
use gemini_integration::{GeminiAppState, create_gemini_client};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO) // Set to DEBUG to see more logs
        .with_target(true)
        .init();

    info!("🚀 Starting Gemini Voice Assistant...");
    let app_config = Config::from_env().expect("🚨 Failed to load configuration from .env");

    // Audio Input (ESP32 Mic -> Rust TCP Client) - Unchanged
    let mut audio_input_source: Box<dyn AsyncAudioInput> =
        match app_config.audio_source.to_uppercase().as_str() {
            "TCP" => Box::new(MicTcpInput::new(
                // ESP32 Mic is a TCP Server, Rust is TCP Client
                app_config
                    .tcp_server_address
                    .clone()
                    .expect("TCP_SERVER_ADDRESS for mic input missing"),
                app_config.audio_sample_rate,
                app_config.audio_channels,
            )),
            "MIC" => Box::new(MicAudioInput::new(
                app_config.audio_sample_rate,
                app_config.audio_channels,
            )),
            _ => return Err(anyhow::anyhow!("Invalid AUDIO_SOURCE.")),
        };

    // This Crossbeam channel is where Gemini sends its audio output
    let (ai_audio_tx_crossbeam, ai_audio_rx_crossbeam) =
        crossbeam_channel::bounded::<Vec<i16>>(100);

    let gemini_app_state_playback_sender: Option<CrossbeamSender<Vec<i16>>>;

    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    // Store the TcpAudioOutput instance directly
    let mut tcp_audio_output_module: Option<TcpAudioOutput> = None; // <<< CHANGED from Udp

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!("🔊 Using local Speaker audio output (via crossbeam channel).");
            _speaker_playback_manager = Some(SpeakerPlayback::new(ai_audio_rx_crossbeam)?);
            gemini_app_state_playback_sender = Some(ai_audio_tx_crossbeam);
        }
        "TCP_SPEAKER" => {
            let esp32_speaker_tcp_addr = app_config
                .tcp_server_address
                .clone()
                .expect("TARGET_ADDRESS for ESP32 Speaker TCP output missing");

            info!(
                "🔊 Using TCP audio output to ESP32 Speaker at: {}",
                esp32_speaker_tcp_addr
            );

            let mut tcp_module = TcpAudioOutput::new(esp32_speaker_tcp_addr);

            // Start the TcpAudioOutput, giving it the receiver end of the crossbeam channel
            tcp_module
                .start_with_crossbeam_receiver(
                    ai_audio_rx_crossbeam,        // Audio from Gemini
                    app_config.audio_sample_rate, // Or AI_OUTPUT_SAMPLE_RATE_HZ
                    app_config.audio_channels,    // Or AI_OUTPUT_CHANNELS
                )
                .await
                .context("Failed to start TCP output (to ESP32 Speaker) with Crossbeam receiver")?;

            tcp_audio_output_module = Some(tcp_module); // Store the instance

            // GeminiAppState will send to this sender. TcpAudioOutput now directly receives from ai_audio_rx_crossbeam.
            gemini_app_state_playback_sender = Some(ai_audio_tx_crossbeam.clone());

            info!(
                "[Main] TCP output (to ESP32 Speaker) configured to directly consume from Crossbeam channel."
            );
        }
        "NONE" | "" => {
            info!("🔇 Audio output is disabled.");
            gemini_app_state_playback_sender = None;
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid AUDIO_OUTPUT_TYPE. Use SPEAKER, TCP_SPEAKER, or NONE."
            ));
        }
    }

    let initial_prompt = Some(
        "You are a voice-activated assistant. Please speak clearly and concisely. Use tools when appropriate. Respond with both text and speech.".to_string()
    );

    let mut gemini_client = create_gemini_client(
        &app_config,
        initial_prompt,
        gemini_app_state_playback_sender,
    )
    .await?;
    let gemini_app_state_clone: Arc<GeminiAppState> = gemini_client.state();

    let mut audio_chunk_receiver_tokio: TokioReceiver<Vec<i16>> =
        audio_input_source.start_stream().await?;
    info!("🎧 Audio input stream started. Listening for audio chunks...");
    info!("🗣️  Assistant is now listening. Press Ctrl+C to exit.");

    loop {
        // Main application loop
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
                            debug!("➡️ Received MIC audio frame ({} samples). Forwarding to Gemini.", audio_chunk_vec.len());
                            gemini_app_state_clone.current_turn_text.lock().unwrap().clear(); // Clear previous turn's text
                            if let Err(e) = gemini_client.send_audio_chunk(
                                &audio_chunk_vec,
                                app_config.audio_sample_rate, // Sample rate of mic input
                                app_config.audio_channels,
                            ).await {
                                error!("🚨 Failed to send MIC audio to Gemini: {}.", e);
                            }
                        }
                    }
                    None => {
                        info!("🔇 Audio input stream (mic/tcp_mic) ended. Shutting down.");
                        break; // Exit main loop if audio input closes
                    }
                }
            }

            _ = gemini_app_state_clone.turn_complete_signal.notified() => {
                info!("[MainLoop] Gemini turn complete signaled. Ready for new input.");
                // Potentially clear/reset things for the next turn if needed
            }
        }
    }

    // --- Shutdown Sequence ---
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

    // Shutdown TCP output to ESP32 speaker if it was used
    if let Some(mut tcp_module) = tcp_audio_output_module.take() {
        info!("🔌 Shutting down TCP audio output (to ESP32 Speaker)...");
        if let Err(e) = tcp_module.stop_stream_processing().await {
            error!("🚨 Error stopping TCP audio output: {}", e);
        }
    }

    info!("🤖 Closing Gemini client connection...");
    if let Err(e) = gemini_client.close().await {
        // This should lead to GeminiAppState being dropped and Crossbeam channel closing.
        error!("🚨 Error closing Gemini client: {}", e);
    }

    info!("👋 Gemini Voice Assistant shut down gracefully. Goodbye!");
    Ok(())
}
