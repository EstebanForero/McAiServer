use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender as CrossbeamSender;
use std::{fs, sync::Arc, time::Duration};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{Level, debug, error, info};

mod audio_input;
mod audio_output;
mod config;
mod openai_integration;

use audio_input::{
    AsyncAudioInput, mic_input::MicAudioInput, tcp_input::TcpAudioInput as MicTcpInput,
};
use audio_output::{speaker_output::SpeakerPlayback, tcp_output::TcpAudioOutput};

use config::Config;
use openai_integration::{OpenAiAppState, create_openai_client};

fn get_initial_prompt() -> Option<String> {
    fs::read_to_string("prompt.txt").ok()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(true)
        .init();

    info!("🚀 Starting OpenAI Voice Assistant...");
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

    let openai_app_state_playback_sender: Option<CrossbeamSender<Vec<i16>>>;

    let mut _speaker_playback_manager: Option<SpeakerPlayback> = None;
    let mut tcp_audio_output_module: Option<TcpAudioOutput> = None;

    match app_config.audio_output_type.to_uppercase().as_str() {
        "SPEAKER" => {
            info!("🔊 Using local Speaker audio output (via crossbeam channel).");
            _speaker_playback_manager = Some(
                SpeakerPlayback::new(ai_audio_rx_crossbeam)
                    .context("Failed to initialize SpeakerPlayback")?,
            );
            openai_app_state_playback_sender = Some(ai_audio_tx_crossbeam);
        }
        "TCP_SPEAKER" => {
            let target_addr = app_config.udp_output_address.clone().expect(
                "TARGET_ADDRESS (from UDP_OUTPUT_ADDRESS) for ESP32 Speaker TCP output missing",
            );

            info!(
                "🔊 Using TCP audio output to ESP32 Speaker at: {}",
                target_addr
            );

            let mut tcp_module = TcpAudioOutput::new(target_addr);
            tcp_module
                .start_with_crossbeam_receiver(
                    ai_audio_rx_crossbeam,
                    openai_integration::AI_OUTPUT_SAMPLE_RATE_HZ,
                    openai_integration::AI_OUTPUT_CHANNELS,
                )
                .await
                .context("Failed to start TCP output (to ESP32 Speaker) with Crossbeam receiver")?;
            tcp_audio_output_module = Some(tcp_module);
            openai_app_state_playback_sender = Some(ai_audio_tx_crossbeam.clone());
            info!("[Main] TCP output (to ESP32 Speaker) configured.");
        }
        "NONE" | "" => {
            info!("🔇 Audio output is disabled.");
            openai_app_state_playback_sender = None;
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid AUDIO_OUTPUT_TYPE. Use SPEAKER, TCP_SPEAKER, or NONE."
            ));
        }
    }

    let initial_prompt = get_initial_prompt();

    let mut ai_client = create_openai_client(
        // <<< CHANGED
        &app_config,
        initial_prompt,
        openai_app_state_playback_sender,
    )
    .await?;
    let ai_app_state_clone: Arc<OpenAiAppState> = ai_client.state();

    let mut audio_chunk_receiver_tokio: TokioReceiver<Vec<i16>> =
        audio_input_source.start_stream().await?;
    info!("🎧 Audio input stream started. Listening for audio chunks...");
    info!("🗣️  Assistant is now listening. Press Ctrl+C to exit.");

    let mut last_audio_sent_time = tokio::time::Instant::now();
    const VAD_SILENCE_DURATION: Duration = Duration::from_secs(2);

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
                            debug!("➡️ Received MIC audio frame ({} samples). Forwarding to OpenAI.", audio_chunk_vec.len());

                            if let Err(e) = ai_client.send_audio_chunk(
                                &audio_chunk_vec,
                                app_config.audio_sample_rate,
                            ).await {
                                error!("🚨 Failed to send MIC audio to OpenAI: {}.", e);
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
                        break;
                    }
                }
            }

            _ = tokio::time::sleep_until(last_audio_sent_time + VAD_SILENCE_DURATION), if tokio::time::Instant::now() >= last_audio_sent_time + VAD_SILENCE_DURATION => {
                info!("[VAD] Silence detected for {}s. Sending audio_stream_end to OpenAI.", VAD_SILENCE_DURATION.as_secs());
                if let Err(e) = ai_client.send_audio_stream_end().await {
                    error!("🚨 Failed to send audio_stream_end (VAD) to OpenAI: {}.", e);
                     if matches!(e, gemini_live_api::error::GeminiError::SendError | gemini_live_api::error::GeminiError::ConnectionClosed) {
                        error!("Connection lost after VAD. Shutting down.");
                        break;
                    }
                }
                last_audio_sent_time = tokio::time::Instant::now() + Duration::from_secs(1000);
            }


            _ = ai_app_state_clone.turn_complete_signal.notified() => {
                info!("[MainLoop] OpenAI turn complete signaled. Ready for new input.");
                ai_app_state_clone.current_turn_text.lock().unwrap().clear();
                last_audio_sent_time = tokio::time::Instant::now();
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

    if let Some(mut tcp_module) = tcp_audio_output_module.take() {
        info!("🔌 Shutting down TCP audio output (to ESP32 Speaker)...");
        if let Err(e) = tcp_module.stop_stream_processing().await {
            error!("🚨 Error stopping TCP audio output: {}", e);
        }
    }

    info!("🤖 Closing OpenAI client connection...");
    if !ai_client.is_closed() {
        // Check if already closed
        if let Err(e) = ai_client.close().await {
            error!("🚨 Error closing OpenAI client: {}", e);
        }
    }

    info!("👋 OpenAI Voice Assistant shut down gracefully. Goodbye!");
    Ok(())
}
