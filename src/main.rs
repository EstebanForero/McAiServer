use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender}; // Ensure Sender is imported
use tracing::{debug, error, info, warn};

mod audio_input;
mod audio_output; // Added
mod config;
mod gemini_integration;

use audio_input::{AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput};
use audio_output::{
    AsyncAudioOutput, speaker_output::SpeakerAudioOutput, udp_output::UdpAudioOutput,
};
use config::Config;
use gemini_integration::{GeminiAppState, create_gemini_client};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .init();

    info!("🚀 Starting Gemini Voice Assistant...");
    let app_config = Config::from_env().expect("🚨 Failed to load configuration from .env");
    info!(
        "Configuration loaded. Audio Input: {}, Audio Output: {}",
        app_config.audio_source, app_config.audio_output_type
    );

    // --- Initialize Audio Input Source ---
    let mut audio_input_source: Box<dyn AsyncAudioInput> =
        match app_config.audio_source.to_uppercase().as_str() {
            "TCP" => {
                let tcp_addr = app_config
                    .tcp_server_address
                    .clone()
                    .expect("TCP_SERVER_ADDRESS missing for TCP source");
                info!("🎤 Using TCP audio input from: {}", tcp_addr);
                Box::new(TcpAudioInput::new(
                    tcp_addr,
                    app_config.audio_sample_rate,
                    app_config.audio_channels,
                ))
            }
            "MIC" => {
                info!("🎤 Using Microphone audio input.");
                Box::new(MicAudioInput::new(
                    app_config.audio_sample_rate,
                    app_config.audio_channels,
                ))
            }
            _ => {
                error!(
                    "🚨 Invalid AUDIO_SOURCE: '{}'. Supported: TCP, MIC.",
                    app_config.audio_source
                );
                return Err(anyhow::anyhow!("Invalid AUDIO_SOURCE configuration."));
            }
        };

    // --- Initialize Audio Output Sink ---
    let mut audio_output_sink: Option<Box<dyn AsyncAudioOutput>> = None;
    let audio_output_sender: Option<Sender<Vec<i16>>>; // To send audio data for output

    match app_config.audio_output_type.to_uppercase().as_str() {
        "UDP" => {
            let udp_addr = app_config
                .udp_output_address
                .clone()
                .expect("UDP_OUTPUT_ADDRESS missing for UDP output");
            info!("🔊 Using UDP audio output to: {}", udp_addr);
            let mut sink = UdpAudioOutput::new(udp_addr);
            audio_output_sender = Some(
                sink.start_stream(app_config.audio_sample_rate, app_config.audio_channels)
                    .await?,
            );
            audio_output_sink = Some(Box::new(sink));
        }
        "SPEAKER" => {
            info!("🔊 Using Speaker audio output.");
            let mut sink = SpeakerAudioOutput::new();
            audio_output_sender = Some(
                sink.start_stream(app_config.audio_sample_rate, app_config.audio_channels)
                    .await?,
            );
            audio_output_sink = Some(Box::new(sink));
        }
        "NONE" | "" => {
            info!("🔇 Audio output is disabled.");
            audio_output_sender = None;
        }
        _ => {
            error!(
                "🚨 Invalid AUDIO_OUTPUT_TYPE: '{}'. Supported: UDP, SPEAKER, NONE.",
                app_config.audio_output_type
            );
            return Err(anyhow::anyhow!("Invalid AUDIO_OUTPUT_TYPE configuration."));
        }
    }

    // --- Initialize Gemini Client ---
    let initial_prompt = Some(
        "You are a voice-activated assistant. Please speak clearly and concisely. Use tools when appropriate.".to_string()
    );
    let mut gemini_client = create_gemini_client(&app_config, initial_prompt).await?;
    let gemini_app_state_clone: Arc<GeminiAppState> = gemini_client.state();

    // --- Start Audio Stream from Input ---
    let mut audio_chunk_receiver: Receiver<Vec<i16>> = audio_input_source.start_stream().await?;
    info!("🎧 Audio input stream started. Listening for audio chunks...");

    info!("🗣️  Assistant is now listening. Press Ctrl+C to exit.");

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                info!("🛑 Ctrl+C received. Initiating shutdown...");
                break;
            }

            audio_chunk_option = audio_chunk_receiver.recv() => {
                match audio_chunk_option {
                    Some(audio_chunk) => {
                        if !audio_chunk.is_empty() {
                            debug!("➡️ Received audio frame ({} samples) from input.", audio_chunk.len());

                            // 1. Send to Gemini
                            gemini_app_state_clone.current_turn_text.lock().unwrap().clear(); // Clear for new utterance
                            if let Err(e) = gemini_client.send_audio_chunk(audio_chunk.clone()).await { // Clone for output
                                error!("🚨 Failed to send audio frame to Gemini: {}. Halting audio sending to Gemini.", e);
                                // Potentially break or implement reconnection logic for Gemini
                            }

                            // 2. Send to configured Audio Output (e.g., for monitoring/passthrough)
                            if let Some(sender) = &audio_output_sender {
                                if sender.send(audio_chunk).await.is_err() {
                                    // This error means the audio output task has ended.
                                    // It might be okay if it's a graceful shutdown of that task.
                                    warn!("🎤🔊 Failed to send audio chunk to output sink (receiver dropped). Output may have stopped.");
                                    // Consider logic to re-establish output if desired, or mark sender as dead.
                                }
                            }
                        }
                    }
                    None => {
                        info!("🔇 Audio input stream from source has ended. Exiting main loop.");
                        break;
                    }
                }
            }

            _ = gemini_app_state_clone.turn_complete_signal.notified() => {
                let final_text_this_turn = gemini_app_state_clone.current_turn_text.lock().unwrap().clone();
                if !final_text_this_turn.trim().is_empty() {
                    info!("[App] Gemini turn processing complete. Accumulated text for turn: \"{}\"", final_text_this_turn.trim());
                } else {
                    info!("[App] Gemini turn processing complete (no new text or tool-only turn).");
                }
                // TODO: If you implement TTS, this is where you'd get `final_text_this_turn`
                // and send its audio representation to `audio_output_sender`.
                // For now, only input audio is passed to the output.
            }
        }
    }

    // --- Graceful Shutdown ---
    info!("🔌 Shutting down audio input source...");
    if let Err(e) = audio_input_source.stop_stream().await {
        error!("🚨 Error stopping audio input source: {}", e);
    }

    if let Some(mut sink) = audio_output_sink.take() {
        info!("🔌 Shutting down audio output sink...");
        // The sender channel (audio_output_sender) will be dropped when it goes out of scope here or earlier if send fails.
        // This signals the output tasks to complete processing buffered data and then terminate.
        drop(audio_output_sender); // Explicitly drop sender to signal completion
        if let Err(e) = sink.stop_stream().await {
            error!("🚨 Error stopping audio output sink: {}", e);
        }
    }

    info!("🤖 Closing Gemini client connection...");
    if let Err(e) = gemini_client.close().await {
        error!("🚨 Error closing Gemini client: {}", e);
    }

    info!("👋 Gemini Voice Assistant shut down gracefully. Goodbye!");
    Ok(())
}
