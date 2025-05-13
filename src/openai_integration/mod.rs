// src/openai_integration/mod.rs
use anyhow::Result;
use crossbeam_channel::Sender as CrossbeamSender;
use gemini_live_api::{
    AiClientBuilder,                                                    // <<< CHANGED
    client::{AiLiveClient, ServerContentContext, UsageMetadataContext}, // <<< AiLiveClient
    types::*,
};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

mod tools; // Assuming tools.rs is also moved and adapted
use super::openai_integration::tools::register_all_tools; // <<< CHANGED
use crate::config::Config;

// OpenAI output is typically 24kHz
pub const AI_OUTPUT_SAMPLE_RATE_HZ: u32 = 24000;
pub const AI_OUTPUT_CHANNELS: u16 = 1;

#[derive(Clone, Debug)]
pub struct OpenAiAppState {
    // <<< CHANGED
    pub current_turn_text: Arc<StdMutex<String>>,
    pub turn_complete_signal: Arc<Notify>,
    pub ai_audio_playback_sender: Option<Arc<CrossbeamSender<Vec<i16>>>>,
    pub backend_url: Option<String>, // Made Option, as it's only for tools
}

impl OpenAiAppState {
    // <<< CHANGED
    pub fn new(
        playback_sender: Option<CrossbeamSender<Vec<i16>>>,
        backend_url: Option<String>,
    ) -> Self {
        Self {
            current_turn_text: Arc::new(StdMutex::new(String::new())),
            turn_complete_signal: Arc::new(Notify::new()),
            ai_audio_playback_sender: playback_sender.map(Arc::new),
            backend_url,
        }
    }
}

// Adapted for OpenAI's ServerContentContext structure
async fn handle_openai_content(ctx: ServerContentContext, app_state: Arc<OpenAiAppState>) {
    // <<< CHANGED
    // ctx directly provides text, audio, is_done
    if let Some(text_segment) = ctx.text {
        if !text_segment.is_empty() {
            print!("{}", text_segment); // OpenAI sends deltas, print directly
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
        if !current_text.trim().is_empty() {
            println!(); // Newline after full text response
            std::io::stdout().flush().unwrap_or_default();
        }
        info!("[Handler] OpenAI content segment processing complete (is_done=true).");
        // For OpenAI, `is_done` here means a text or audio segment is done.
        // The actual "turn" completion is signaled by a ModelTurnComplete event
        // which is handled by the library and can trigger client.state().turn_complete_signal
        // or a dedicated handler if you add one to the builder.
        // For simplicity here, we'll assume is_done might imply a point where UI can update.
        // The main loop's turn_complete_signal will be notified by the library's internal handling of ModelTurnComplete.
        app_state.turn_complete_signal.notify_one(); // Or rely on client's internal notification
    }
}

async fn handle_openai_usage_metadata(ctx: UsageMetadataContext, _app_state: Arc<OpenAiAppState>) {
    // <<< CHANGED
    info!("[Handler] OpenAI Usage Metadata: {:?}", ctx.metadata);
}

pub async fn create_openai_client(
    // <<< CHANGED
    app_config: &Config,
    initial_prompt_text: Option<String>,
    ai_audio_playback_sender: Option<CrossbeamSender<Vec<i16>>>,
) -> Result<AiLiveClient<OpenAiAppState, gemini_live_api::client::openai_backend::OpenAiBackend>> {
    // More specific client type
    let openai_app_state = OpenAiAppState::new(
        // <<< CHANGED
        ai_audio_playback_sender,
        app_config.backend_url.clone(),
    );

    info!(
        "Configuring OpenAI Live Client for model: {}", // <<< CHANGED
        app_config.openai_model_name                    // <<< CHANGED
    );
    let mut builder = AiClientBuilder::<OpenAiAppState>::new_with_model_and_state(
        // <<< CHANGED
        app_config.openai_api_key.clone(),    // <<< CHANGED
        app_config.openai_model_name.clone(), // <<< CHANGED
        openai_app_state.clone(),
    );

    builder = builder.generation_config(GenerationConfig {
        response_modalities: Some(vec![ResponseModality::Audio, ResponseModality::Text]),
        temperature: Some(0.7),
        // No speech_config for OpenAI here; use .voice()
        ..Default::default()
    });

    // OpenAI specific settings
    builder = builder.voice("alloy".to_string()); // Example voice
    builder = builder.input_audio_format("pcm16".to_string()); // OpenAI expects this for raw
    builder = builder.output_audio_format("pcm16".to_string()); // Request pcm16 output

    // No RealtimeInputConfig for OpenAI in this library's abstraction; VAD is different.
    // builder = builder.realtime_input_config(...)

    builder = builder.output_audio_transcription(AudioTranscriptionConfig {}); // Enable transcription

    let system_message = initial_prompt_text.unwrap_or_else(||
        "You are a helpful voice assistant for McDonald's that takes orders and can use tools. Please respond in Spanish.".to_string()
    );
    info!("Using system instruction: \"{}\"", system_message);
    builder = builder.system_instruction(Content {
        parts: vec![Part {
            text: Some(system_message),
            ..Default::default()
        }],
        role: Some(Role::System), // Make sure Role::System is used
    });

    builder = builder.on_server_content(handle_openai_content); // <<< CHANGED
    builder = builder.on_usage_metadata(handle_openai_usage_metadata); // <<< CHANGED
    builder = register_all_tools(builder); // Assumes tools.rs is adapted

    info!("Connecting OpenAI client..."); // <<< CHANGED
    let client = builder.connect_openai().await?; // <<< CHANGED to connect_openai
    info!("OpenAI client connected successfully."); // <<< CHANGED
    Ok(client)
}
