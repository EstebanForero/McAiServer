use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{Level, debug, error, info, warn};

mod audio_input;
mod audio_output;
mod config;
mod gemini_integration;

use audio_input::{AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput};
use audio_output::speaker_output::SpeakerPlayback;
use audio_output::{AsyncAudioOutput, udp_output::UdpAudioOutput};

use config::Config;
use gemini_integration::{GeminiAppState, create_gemini_client};

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

    let (ai_audio_tx_crossbeam, ai_audio_rx_crossbeam) =
        crossbeam_channel::bounded::<Vec<i16>>(100);

    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    let mut _udp_output_sink: Option<Box<dyn AsyncAudioOutput>> = None;

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!("🔊 Using Speaker audio output (via crossbeam channel).");
            _speaker_playback_manager = Some(SpeakerPlayback::new(ai_audio_rx_crossbeam)?);
        }
        "UDP" => {
            let udp_addr = app_config
                .udp_output_address
                .clone()
                .expect("UDP_OUTPUT_ADDRESS missing");
            info!("🔊 Using UDP audio output to: {}", udp_addr);
            let mut sink = UdpAudioOutput::new(udp_addr);
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

    let initial_prompt = Some(
        "You are a voice-activated assistant. Please speak clearly and concisely. Use tools when appropriate. Respond with both text and speech.".to_string()
    );

    let mut gemini_client = create_gemini_client(
        &app_config,
        initial_prompt,
        Some(ai_audio_tx_crossbeam.clone()),
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
            _ = tokio::signal::ctrl_c() => { break; }

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

    info!("🔌 Shutting down audio input source...");
    if let Err(e) = audio_input_source.stop_stream().await {
        error!("🚨 Error stopping audio input source: {}", e);
    }

    if let Some(mut spm) = _speaker_playback_manager.take() {
        info!("🔌 Shutting down speaker playback manager...");
        drop(ai_audio_tx_crossbeam);
        if let Err(e) = spm.stop() {
            error!("🚨 Error stopping speaker playback: {}", e);
        }
    }

    if let Some(mut sink) = _udp_output_sink.take() {
        info!("🔌 Shutting down UDP audio output sink...");
        if let Err(e) = sink.stop_stream().await {
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
