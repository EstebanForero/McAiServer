use anyhow::Result;
use base64::Engine as _;
use crossbeam_channel::Sender as CrossbeamSender;
use gemini_live_api::{
    GeminiLiveClient, GeminiLiveClientBuilder,
    client::{ServerContentContext, UsageMetadataContext},
    types::*,
};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

mod tools;
use super::gemini_integration::tools::register_all_tools;
use crate::config::Config;

pub const AI_OUTPUT_SAMPLE_RATE_HZ: u32 = 24000;
pub const AI_OUTPUT_CHANNELS: u16 = 1;

#[derive(Clone, Debug)]
pub struct GeminiAppState {
    pub current_turn_text: Arc<StdMutex<String>>,
    pub turn_complete_signal: Arc<Notify>,
    pub ai_audio_playback_sender: Option<Arc<CrossbeamSender<Vec<i16>>>>,
    pub backend_url: String,
}

impl GeminiAppState {
    pub fn new(playback_sender: Option<CrossbeamSender<Vec<i16>>>, backend_url: String) -> Self {
        Self {
            current_turn_text: Arc::new(StdMutex::new(String::new())),
            turn_complete_signal: Arc::new(Notify::new()),
            ai_audio_playback_sender: playback_sender.map(Arc::new),
            backend_url,
        }
    }
}

async fn handle_gemini_content(ctx: ServerContentContext, app_state: Arc<GeminiAppState>) {
    debug!("[Handler] Received content: {:?}", ctx.content);
    let mut new_text_received = false;

    if let Some(model_turn) = &ctx.content.model_turn {
        for part in &model_turn.parts {
            if let Some(text) = &part.text {
                if !text.is_empty() {
                    print!("{}", text);
                    std::io::stdout().flush().unwrap_or_default();
                    app_state.current_turn_text.lock().unwrap().push_str(text);
                    new_text_received = true;
                }
            }
            if let Some(function_call) = &part.function_call {
                info!(
                    "\n[Handler] Requested function call: {} with args {:?}",
                    function_call.name, function_call.args
                );
            }
            if let Some(function_response) = &part.function_response {
                info!(
                    "\n[Handler] Processed function response: {}",
                    function_response.name
                );
            }
            if let Some(blob) = &part.inline_data {
                if blob.mime_type.starts_with("audio/") {
                    debug!(
                        "[Handler] Received audio blob from Gemini (mime_type: {}). Decoding...",
                        blob.mime_type
                    );
                    if let Some(playback_sender_arc) = &app_state.ai_audio_playback_sender {
                        match base64::engine::general_purpose::STANDARD.decode(&blob.data) {
                            Ok(decoded_bytes) => {
                                if decoded_bytes.is_empty() {
                                    warn!("[Handler] Decoded audio from Gemini is empty.");
                                    continue;
                                }
                                let samples: Vec<i16> = decoded_bytes
                                    .chunks_exact(2)
                                    .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                                    .collect();

                                if !samples.is_empty() {
                                    if let Err(e) = playback_sender_arc.send(samples) {
                                        error!(
                                            "[Handler] Error sending AI audio samples to playback channel: {}",
                                            e
                                        );
                                    } else {
                                        debug!(
                                            "[Handler] Sent {} AI audio samples to playback channel.",
                                            decoded_bytes.len() / 2
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!("[Handler] Base64 decode error for Gemini audio: {}", e)
                            }
                        }
                    } else {
                        warn!(
                            "[Handler] Received audio from Gemini, but no playback sender in AppState."
                        );
                    }
                }
            }
        }
    }
    if let Some(transcription) = &ctx.content.output_transcription {
        debug!(
            "[Handler] Model Output Transcription: {}",
            transcription.text
        );
    }

    if ctx.content.turn_complete {
        if new_text_received {
            println!();
            std::io::stdout().flush().unwrap_or_default();
        }
        info!("[Handler] Model turn_complete message received.");
        app_state.current_turn_text.lock().unwrap().clear();
        app_state.turn_complete_signal.notify_one();
    }
}

async fn handle_gemini_usage_metadata(ctx: UsageMetadataContext, _app_state: Arc<GeminiAppState>) {
    info!("[Handler] Usage Metadata: {:?}", ctx.metadata);
}

pub async fn create_gemini_client(
    app_config: &Config,
    initial_prompt_text: Option<String>,
    ai_audio_playback_sender: Option<CrossbeamSender<Vec<i16>>>,
) -> Result<GeminiLiveClient<GeminiAppState>> {
    let gemini_app_state = GeminiAppState::new(
        ai_audio_playback_sender,
        app_config.backend_url.clone().unwrap(),
    );

    info!(
        "Configuring Gemini Live Client for model: {}",
        app_config.gemini_model_name
    );
    let mut builder = GeminiLiveClientBuilder::<GeminiAppState>::new_with_state(
        app_config.gemini_api_key.clone(),
        app_config.gemini_model_name.clone(),
        gemini_app_state.clone(),
    );

    builder = builder.generation_config(GenerationConfig {
        response_modalities: Some(vec![ResponseModality::Audio]),
        temperature: Some(0.7),
        speech_config: Some(SpeechConfig {
            language_code: Some(SpeechLanguageCode::SpanishES),
        }),
        ..Default::default()
    });

    builder = builder.realtime_input_config(RealtimeInputConfig {
        automatic_activity_detection: Some(AutomaticActivityDetection {
            disabled: Some(false),
            silence_duration_ms: Some(500),
            prefix_padding_ms: Some(100),
            start_of_speech_sensitivity: Some(StartSensitivity::StartSensitivityHigh),
            end_of_speech_sensitivity: Some(EndSensitivity::EndSensitivityHigh),
        }),
        activity_handling: Some(ActivityHandling::StartOfActivityInterrupts),
        turn_coverage: Some(TurnCoverage::TurnIncludesOnlyActivity),
    });
    builder = builder.output_audio_transcription(AudioTranscriptionConfig {});
    let system_message = initial_prompt_text.unwrap_or_else(||
        "Eres un asistente que solo habla en espanol, que recive ordenes para un McDonalds, que envia datos de las ordenes con la echo tool, en json".to_string()
    );
    info!("Using system instruction: \"{}\"", system_message);
    builder = builder.system_instruction(Content {
        parts: vec![Part {
            text: Some(system_message),
            ..Default::default()
        }],
        role: Some(Role::System),
    });

    builder = builder.on_server_content(handle_gemini_content);
    builder = builder.on_usage_metadata(handle_gemini_usage_metadata);
    builder = register_all_tools(builder);

    info!("Connecting Gemini client...");
    let client = builder.connect().await?;
    info!("Gemini client connected successfully.");
    Ok(client)
}
