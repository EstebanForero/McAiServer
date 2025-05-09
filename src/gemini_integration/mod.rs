use anyhow::Result;
use base64::Engine as _;
use gemini_live_api::{
    GeminiLiveClient,
    GeminiLiveClientBuilder,
    client::{ServerContentContext, UsageMetadataContext}, // Ensure UsageMetadataContext is used or remove
    types::*,
};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Notify;
// Use crossbeam_channel::Sender for AI audio playback
use crossbeam_channel::Sender as CrossbeamSender;
use tracing::{debug, error, info, warn};

mod tools;
use super::gemini_integration::tools::register_all_tools;
use crate::config::Config;

// Define the output sample rate expected from Gemini / for playback
// This should match what your speaker output will be configured for.
// The official example uses 24000 Hz.
pub const AI_OUTPUT_SAMPLE_RATE_HZ: u32 = 24000;
pub const AI_OUTPUT_CHANNELS: u16 = 1;

#[derive(Clone, Debug)]
pub struct GeminiAppState {
    pub current_turn_text: Arc<StdMutex<String>>,
    pub turn_complete_signal: Arc<Notify>,
    // Sender to send decoded AI audio to the speaker output module (using crossbeam)
    pub ai_audio_playback_sender: Option<Arc<CrossbeamSender<Vec<i16>>>>,
}

impl GeminiAppState {
    pub fn new(playback_sender: Option<CrossbeamSender<Vec<i16>>>) -> Self {
        Self {
            current_turn_text: Arc::new(StdMutex::new(String::new())),
            turn_complete_signal: Arc::new(Notify::new()),
            ai_audio_playback_sender: playback_sender.map(Arc::new),
        }
    }
}

async fn handle_gemini_content(ctx: ServerContentContext, app_state: Arc<GeminiAppState>) {
    // ... (text, function call, function response handling remains the same) ...
    debug!("[Handler] Received content: {:?}", ctx.content); // Keep for debugging
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
                    // Log the exact mime_type to confirm sample rate if provided by Gemini
                    info!(
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
                                // Crucial: Assume Gemini sends LINEAR16 PCM.
                                // The number of samples depends on the actual sample rate of THIS audio.
                                // If blob.mime_type gives a rate, use it. Otherwise, assume AI_OUTPUT_SAMPLE_RATE_HZ.
                                let samples: Vec<i16> = decoded_bytes
                                    .chunks_exact(2)
                                    .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                                    .collect();

                                if !samples.is_empty() {
                                    // Use try_send for non-blocking, or send which might block if buffer is full.
                                    // Given it's from an async handler, try_send or send_timeout is safer.
                                    // The official example uses blocking .send() as it's from an async task
                                    // to a crossbeam_channel that the cpal thread consumes.
                                    if let Err(e) = playback_sender_arc.send(samples) {
                                        // .send() on crossbeam can block
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
    // ... (output_transcription and turn_complete handling remains the same) ...
    if let Some(transcription) = &ctx.content.output_transcription {
        info!(
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
    info!(
        "[Handler] Usage Metadata: {:?}",
        ctx.metadata // Assuming _app_state might be used later for tool counts etc.
    );
}

pub async fn create_gemini_client(
    app_config: &Config,
    initial_prompt_text: Option<String>,
    ai_audio_playback_sender: Option<CrossbeamSender<Vec<i16>>>, // Updated type
) -> Result<GeminiLiveClient<GeminiAppState>> {
    let gemini_app_state = GeminiAppState::new(ai_audio_playback_sender);

    // ... (builder setup, GenerationConfig)
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
        response_modalities: Some(vec![ResponseModality::Audio]), // Request Audio
        temperature: Some(0.7),
        speech_config: Some(SpeechConfig {
            language_code: Some(SpeechLanguageCode::EnglishUS),
            // If your `gemini-live-api` types.rs SpeechConfig allowed specifying output sample rate,
            // you would set it here to AI_OUTPUT_SAMPLE_RATE_HZ.
            // e.g. output_audio_config: Some(AudioOutputConfig { sample_rate_hertz: AI_OUTPUT_SAMPLE_RATE_HZ })
            // Since it doesn't, we rely on Gemini's default (likely 24kHz) or what the mime_type indicates.
            ..Default::default()
        }),
        ..Default::default()
    });

    // ... (RealtimeInputConfig, OutputAudioTranscriptionConfig, SystemInstruction remain same) ...
    builder = builder.realtime_input_config(RealtimeInputConfig {
        automatic_activity_detection: Some(AutomaticActivityDetection {
            disabled: Some(false),
            silence_duration_ms: Some(500),
            prefix_padding_ms: Some(100),
            start_of_speech_sensitivity: Some(StartSensitivity::StartSensitivityHigh),
            end_of_speech_sensitivity: Some(EndSensitivity::EndSensitivityHigh),
            ..Default::default()
        }),
        activity_handling: Some(ActivityHandling::StartOfActivityInterrupts),
        turn_coverage: Some(TurnCoverage::TurnIncludesOnlyActivity),
        ..Default::default()
    });
    builder = builder.output_audio_transcription(AudioTranscriptionConfig {});
    let system_message = initial_prompt_text.unwrap_or_else(||
        "You are a helpful voice assistant. Respond clearly and concisely. Use available tools when needed. Please respond with both text and speech.".to_string()
    );
    info!("Using system instruction: \"{}\"", system_message);
    builder = builder.system_instruction(Content {
        parts: vec![Part {
            text: Some(system_message),
            ..Default::default()
        }],
        role: Some(Role::System),
        ..Default::default()
    });

    builder = builder.on_server_content(handle_gemini_content);
    builder = builder.on_usage_metadata(handle_gemini_usage_metadata); // Add this back
    builder = register_all_tools(builder);

    info!("Connecting Gemini client...");
    let client = builder.connect().await?;
    info!("Gemini client connected successfully.");
    Ok(client)
}
