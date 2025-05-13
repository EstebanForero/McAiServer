use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender as CrossbeamSender;
use std::{fs, sync::Arc};
use tokio::{io, sync::mpsc::Receiver as TokioReceiver};
use tracing::{Level, debug, error, info, warn};

mod audio_input;
mod audio_output;
mod config;
mod openai_integration;

use audio_input::{
    AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput as MicTcpInput,
};
use audio_output::{speaker_output::SpeakerPlayback, tcp_output::TcpAudioOutput}; // <<< CHANGED

use config::Config;
use gemini_integration::{GeminiAppState, create_gemini_client};

fn get_initial_prompt() -> Option<String> {
    let file: Option<String> = fs::read("prompt.txt")
        .ok()
        .map(|file| String::from_utf8_lossy(&file).to_string());

    file
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(true)
        .init();

    info!("🚀 Starting Gemini Voice Assistant...");
    let app_config = Config::from_env().expect("🚨 Failed to load configuration from .env");

    let mut audio_input_source: Box<dyn AsyncAudioInput> =
        match app_config.audio_source.to_uppercase().as_str() {
            "TCP" => Box::new(MicTcpInput::new(
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

    let (ai_audio_tx_crossbeam, ai_audio_rx_crossbeam) =
        crossbeam_channel::bounded::<Vec<i16>>(100);

    let gemini_app_state_playback_sender: Option<CrossbeamSender<Vec<i16>>>;

    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    let mut tcp_audio_output_module: Option<TcpAudioOutput> = None;

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!("🔊 Using local Speaker audio output (via crossbeam channel).");
            _speaker_playback_manager = Some(SpeakerPlayback::new(ai_audio_rx_crossbeam)?);
            gemini_app_state_playback_sender = Some(ai_audio_tx_crossbeam);
        }
        "TCP_SPEAKER" => {
            let esp32_speaker_tcp_addr = app_config
                .udp_output_address
                .clone()
                .expect("TARGET_ADDRESS for ESP32 Speaker TCP output missing");

            info!(
                "🔊 Using TCP audio output to ESP32 Speaker at: {}",
                esp32_speaker_tcp_addr
            );

            let mut tcp_module = TcpAudioOutput::new(esp32_speaker_tcp_addr);

            tcp_module
                .start_with_crossbeam_receiver(
                    ai_audio_rx_crossbeam,
                    app_config.audio_sample_rate,
                    app_config.audio_channels,
                )
                .await
                .context("Failed to start TCP output (to ESP32 Speaker) with Crossbeam receiver")?;

            tcp_audio_output_module = Some(tcp_module);

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

    let initial_prompt = get_initial_prompt();

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
