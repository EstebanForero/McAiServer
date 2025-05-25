// src/openai_integration/mod.rs
use anyhow::Result;
use crossbeam_channel::Sender as CrossbeamSender;
use gemini_live_api::{
    // Assuming this is your top-level library crate
    AiClientBuilder,
    client::{AiLiveClient, ServerContentContext, UsageMetadataContext},
    types::*,
};
use std::io::Write;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
}; // Added AtomicBool
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

mod tools;
use super::openai_integration::tools::register_all_tools;
use crate::config::Config;

pub const AI_OUTPUT_SAMPLE_RATE_HZ: u32 = 24000;
pub const AI_OUTPUT_CHANNELS: u16 = 1;

#[derive(Clone, Debug)]
pub struct OpenAiAppState {
    pub current_turn_text: Arc<StdMutex<String>>,
    pub turn_complete_signal: Arc<Notify>,
    pub ai_audio_playback_sender: Option<Arc<CrossbeamSender<Vec<i16>>>>,
    pub backend_url: Option<String>,
    ai_is_speaking: Arc<AtomicBool>, // To track if AI is currently sending audio
}

impl OpenAiAppState {
    pub fn new(
        playback_sender: Option<CrossbeamSender<Vec<i16>>>,
        backend_url: Option<String>,
    ) -> Self {
        Self {
            current_turn_text: Arc::new(StdMutex::new(String::new())),
            turn_complete_signal: Arc::new(Notify::new()),
            ai_audio_playback_sender: playback_sender.map(Arc::new),
            backend_url,
            ai_is_speaking: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_ai_is_speaking(&self, speaking: bool) {
        self.ai_is_speaking.store(speaking, Ordering::Relaxed);
    }

    pub fn is_ai_speaking(&self) -> bool {
        self.ai_is_speaking.load(Ordering::Relaxed)
    }
}

async fn handle_openai_content(ctx: ServerContentContext, app_state: Arc<OpenAiAppState>) {
    if let Some(text_segment) = ctx.text.clone() {
        if !text_segment.is_empty() {
            app_state.set_ai_is_speaking(true); // AI is "speaking" text
            print!("{}", text_segment);
            std::io::stdout().flush().unwrap_or_default();
            app_state
                .current_turn_text
                .lock()
                .unwrap()
                .push_str(&text_segment);
        }
    }

    if let Some(audio_samples) = ctx.audio {
        if !audio_samples.is_empty() {
            app_state.set_ai_is_speaking(true); // AI is sending audio
            debug!(
                "[Handler] Received {} audio samples from OpenAI.",
                audio_samples.len()
            );
            if let Some(playback_sender_arc) = &app_state.ai_audio_playback_sender {
                if let Err(e) = playback_sender_arc.send(audio_samples) {
                    error!(
                        "[Handler] Error sending AI audio samples to playback: {}",
                        e
                    );
                }
            } else {
                warn!("[Handler] Received audio from OpenAI, but no playback sender.");
            }
        }
    }

    if ctx.is_done {
        let current_text = app_state.current_turn_text.lock().unwrap().clone();
        if !current_text.trim().is_empty() && ctx.text.is_some() {
            // Only add newline if this 'done' was for text
            println!();
            std::io::stdout().flush().unwrap_or_default();
        }
        info!("[Handler] OpenAI content segment processing complete (is_done=true).");
        // This `is_done` signals the end of a particular content type (text or audio chunk).
        // The library itself will detect the end of the *entire* turn (ModelTurnComplete)
        // and notify the `turn_complete_signal` you see in main.rs.
        // We can also set ai_is_speaking to false here if this 'done' means no more audio/text for now.
        // However, there might be multiple 'done' segments. The final 'ModelTurnComplete' is more reliable.
        // For VAD, it's better to rely on the main loop's signal.
        // If this is the *final* segment of a turn (e.g. after all audio and text), then:
        // app_state.set_ai_is_speaking(false);
        // The library should signal turn_complete_signal upon "response.done" or equivalent.
        // For now, let main loop's turn_complete_signal handle setting ai_is_speaking to false.
    }
}

async fn handle_openai_usage_metadata(ctx: UsageMetadataContext, _app_state: Arc<OpenAiAppState>) {
    info!("[Handler] OpenAI Usage Metadata: {:?}", ctx.metadata);
}

pub async fn create_openai_client(
    app_config: &Config,
    initial_prompt_text: Option<String>,
    ai_audio_playback_sender: Option<CrossbeamSender<Vec<i16>>>,
) -> Result<AiLiveClient<OpenAiAppState, gemini_live_api::client::openai_backend::OpenAiBackend>> {
    let openai_app_state =
        OpenAiAppState::new(ai_audio_playback_sender, app_config.backend_url.clone());

    info!(
        "Configuring OpenAI Live Client for model: {}",
        app_config.openai_model_name
    );
    let mut builder = AiClientBuilder::<OpenAiAppState>::new_with_model_and_state(
        app_config.openai_api_key.clone(),
        app_config.openai_model_name.clone(),
        openai_app_state.clone(),
    );

    builder = builder.generation_config(GenerationConfig {
        response_modalities: Some(vec![ResponseModality::Audio, ResponseModality::Text]),
        temperature: Some(0.7),
        ..Default::default()
    });

    builder = builder.voice("alloy".to_string());
    builder = builder.input_audio_format("pcm16".to_string());
    builder = builder.output_audio_format("pcm16".to_string());

    let mut transcription_config = AudioTranscriptionConfig::default();
    if let Some(model) = &app_config.openai_transcription_model {
        transcription_config.model = Some(model.clone());
    }

    if let Some(lang) = &app_config.openai_transcription_language {
        transcription_config.language = Some(lang.clone());
    }

    if transcription_config.model.is_some() || transcription_config.language.is_some() {
        builder = builder.output_audio_transcription(transcription_config);
    } else {
        error!("Error adding model, and language")
    }
    //builder = builder.output_audio_transcription(AudioTranscriptionConfig {});
    if let Some(nr_type) = &app_config.openai_noise_reduction_type {
        info!("Enabling OpenAI noise reduction: {:?}", nr_type);
        builder = builder.input_audio_noise_reduction(InputAudioNoiseReduction {
            r#type: Some(nr_type.clone()), // Pass the cloned type
        });
    } else {
        info!(
            "OpenAI noise reduction not configured (or set to none/null). Defaulting to OpenAI's behavior (likely off)."
        );
        // To explicitly send `null` to turn it off, if OpenAI requires that for disabling vs. default:
        // builder = builder.input_audio_noise_reduction(InputAudioNoiseReduction { r#type: None });
    }

    let system_message = initial_prompt_text.unwrap_or_else(
        || "You are a helpful voice assistant. Respond concisely.".to_string(), // Generic prompt
    );
    info!("Using system instruction: \"{}\"", system_message);
    builder = builder.system_instruction(Content {
        parts: vec![Part {
            text: Some(system_message),
            ..Default::default()
        }],
        role: Some(Role::System),
    });

    builder = builder.on_server_content(handle_openai_content);
    builder = builder.on_usage_metadata(handle_openai_usage_metadata);
    builder = builder.transcription_model("whisper-1".to_string());
    builder = builder.transcription_language("es".to_string());
    builder = register_all_tools(builder);

    info!("Connecting OpenAI client...");
    let client = builder.connect_openai().await?;
    info!("OpenAI client connected successfully.");
    Ok(client)
}
