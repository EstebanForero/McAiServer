use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver as TokioReceiver;
// No longer need TokioSender for speaker if using crossbeam directly
// use tokio::sync::mpsc::Sender as TokioSender;
use tracing::{debug, error, info, warn};

mod audio_input;
mod audio_output; // Keep for UDP if needed
mod config;
mod gemini_integration;

use audio_input::{AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput};
// If UDP is still needed:
use audio_output::{AsyncAudioOutput, udp_output::UdpAudioOutput};
// Import the new SpeakerPlayback
use audio_output::speaker_output::SpeakerPlayback;

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

    // --- Initialize Audio Input Source ---
    let mut audio_input_source: Box<dyn AsyncAudioInput> =
        match app_config.audio_source.to_uppercase().as_str() {
            "TCP" => Box::new(TcpAudioInput::new(
                app_config
                    .tcp_server_address
                    .clone()
                    .expect("TCP_SERVER_ADDRESS missing"),
                app_config.audio_sample_rate,
                app_config.audio_channels,
            )),
            "MIC" => Box::new(MicAudioInput::new(
                app_config.audio_sample_rate,
                app_config.audio_channels,
            )),
            _ => return Err(anyhow::anyhow!("Invalid AUDIO_SOURCE.")),
        };

    // --- Initialize Audio Output ---
    // Crossbeam channel for AI audio playback (Gemini -> Speaker)
    let (ai_audio_tx_crossbeam, ai_audio_rx_crossbeam) =
        crossbeam_channel::bounded::<Vec<i16>>(100);

    // This will hold the SpeakerPlayback instance if speaker output is chosen
    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    // This will hold the UDP output sink if UDP output is chosen
    let mut _udp_output_sink: Option<Box<dyn AsyncAudioOutput>> = None;

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!("🔊 Using Speaker audio output (via crossbeam channel).");
            // SpeakerPlayback is given the receiver end of the AI audio channel.
            // Gemini's audio will be sent to ai_audio_tx_crossbeam by handle_gemini_content.
            _speaker_playback_manager = Some(SpeakerPlayback::new(ai_audio_rx_crossbeam)?);
        }
        "UDP" => {
            let udp_addr = app_config
                .udp_output_address
                .clone()
                .expect("UDP_OUTPUT_ADDRESS missing");
            info!("🔊 Using UDP audio output to: {}", udp_addr);
            let mut sink = UdpAudioOutput::new(udp_addr);
            // If using UDP for AI audio, GeminiAppState needs this sender.
            // For now, let's assume UDP output is for something else or not primary for AI voice.
            // If you want AI voice over UDP, you'd need to adapt GeminiAppState.
            // For simplicity, let's assume UDP is for monitoring or a secondary stream for now.
            // If you want AI audio over UDP, you'd need to pass a TokioSender to GeminiAppState for UDP
            // and handle it in `handle_gemini_content` similar to how playback_sender was handled.
            // The `start_stream` returns a TokioSender.
            let _udp_sender_for_other_audio = sink
                .start_stream(app_config.audio_sample_rate, app_config.audio_channels)
                .await?;
            _udp_output_sink = Some(Box::new(sink));
            warn!(
                "UDP output is configured, but AI voice will go to speaker if SPEAKER is also chosen or by default. Ensure `ai_audio_playback_sender` in `GeminiAppState` points to the correct sink if UDP is primary for AI voice."
            );
        }
        "NONE" | "" => {
            info!("🔇 Audio output is disabled.");
        }
        _ => return Err(anyhow::anyhow!("Invalid AUDIO_OUTPUT_TYPE.")),
    }

    // --- Initialize Gemini Client ---
    let initial_prompt = Some(
        "You are a voice-activated assistant. Please speak clearly and concisely. Use tools when appropriate. Respond with both text and speech.".to_string()
    );
    // Pass the SENDER end of the crossbeam channel to Gemini client/state
    let mut gemini_client = create_gemini_client(
        &app_config,
        initial_prompt,
        Some(ai_audio_tx_crossbeam.clone()), // Pass the sender
    )
    .await?;
    let gemini_app_state_clone: Arc<GeminiAppState> = gemini_client.state();

    let mut audio_chunk_receiver_tokio: TokioReceiver<Vec<i16>> =
        audio_input_source.start_stream().await?;
    info!("🎧 Audio input stream started. Listening for audio chunks...");
    info!("🗣️  Assistant is now listening. Press Ctrl+C to exit.");

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => { /* ... shutdown ... */ break; }

            audio_chunk_option = audio_chunk_receiver_tokio.recv() => {
                match audio_chunk_option {
                    Some(audio_chunk_vec) => {
                        if !audio_chunk_vec.is_empty() {
                            debug!("➡️ Received MIC audio frame ({} samples).", audio_chunk_vec.len());
                            gemini_app_state_clone.current_turn_text.lock().unwrap().clear();
                            if let Err(e) = gemini_client.send_audio_chunk(
                                &audio_chunk_vec,
                                app_config.audio_sample_rate,
                                app_config.audio_channels,
                            ).await {
                                error!("🚨 Failed to send MIC audio to Gemini: {}.", e);
                            }
                        }
                    }
                    None => { info!("🔇 Audio input stream ended."); break; }
                }
            }

            _ = gemini_app_state_clone.turn_complete_signal.notified() => {
                info!("[MainLoop] Gemini turn complete signaled.");
            }
        }
    }

    // --- Graceful Shutdown ---
    info!("🔌 Shutting down audio input source...");
    if let Err(e) = audio_input_source.stop_stream().await {
        error!("🚨 Error stopping audio input source: {}", e);
    }

    // Shutdown SpeakerPlayback if it was created
    if let Some(spm) = _speaker_playback_manager.take() {
        info!("🔌 Shutting down speaker playback manager...");
        // The sender (ai_audio_tx_crossbeam) will be dropped when `gemini_app_state_clone` (and thus `gemini_client`) is dropped,
        // or when this main scope ends if we drop our clone. This signals the SpeakerPlayback's feed thread to end.
        drop(ai_audio_tx_crossbeam); // Explicitly drop our clone of the sender
        if let Err(e) = spm.stop() {
            // Call the explicit stop method
            error!("🚨 Error stopping speaker playback: {}", e);
        }
    }

    // Shutdown UDP output if it was created
    if let Some(mut sink) = _udp_output_sink.take() {
        info!("🔌 Shutting down UDP audio output sink...");
        if let Err(e) = sink.stop_stream().await {
            // Async stop
            error!("🚨 Error stopping UDP audio output sink: {}", e);
        }
    }

    info!("🤖 Closing Gemini client connection...");
    if let Err(e) = gemini_client.close().await {
        error!("🚨 Error closing Gemini client: {}", e);
    }

    info!("👋 Gemini Voice Assistant shut down gracefully. Goodbye!");
    Ok(())
}
