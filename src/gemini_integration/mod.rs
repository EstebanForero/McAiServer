use anyhow::Result;
use gemini_live_api::{
    GeminiLiveClient,
    GeminiLiveClientBuilder,
    client::{ServerContentContext, UsageMetadataContext},
    types::*, // Assuming AudioConfig, Part, etc., are here
};
use std::io::Write; // For flushing stdout
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Notify;
use tracing::{debug, error, info}; // Removed unused warn

mod tools;

use super::gemini_integration::tools::register_all_tools;
use crate::config::Config;

// tools module is now part of this file if it's small, or keep as mod tools;
// For simplicity, I'll assume tools.rs is separate and correctly imported.
// Make sure `mod tools;` is present if tools.rs is a separate file in this directory.

#[derive(Clone, Default, Debug)]
pub struct GeminiAppState {
    pub current_turn_text: Arc<StdMutex<String>>,
    pub turn_complete_signal: Arc<Notify>,
}

async fn handle_gemini_content(ctx: ServerContentContext, app_state: Arc<GeminiAppState>) {
    debug!(
        "[Gemini Content Handler] Received content: {:?}",
        ctx.content
    );
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
            // Corrected field names based on common Gemini API patterns / error messages
            if let Some(function_call) = &part.function_call {
                // WAS: tool_call
                info!(
                    "\n[Gemini Content Handler] Requested function call: {} with args {:?}",
                    function_call.name, function_call.args
                );
            }
            // Corrected field names
            if let Some(function_response) = &part.function_response {
                // WAS: tool_response
                info!(
                    "\n[Gemini Content Handler] Processed function response for ID: {}",
                    function_response.name // Assuming name is part of FunctionResponse or an ID is available
                                           // Adjust if FunctionResponse structure is different
                );
            }
        }
    }

    if ctx.content.turn_complete {
        if new_text_received {
            println!();
            std::io::stdout().flush().unwrap_or_default();
        }
        info!("[Gemini Content Handler] Turn complete signaled by Gemini.");
        app_state.turn_complete_signal.notify_one();
    }
}

async fn handle_gemini_usage_metadata(ctx: UsageMetadataContext, _app_state: Arc<GeminiAppState>) {
    info!(
        "[Gemini Usage Handler] Received Usage Metadata: {:?}",
        ctx.metadata
    );
}

pub async fn create_gemini_client(
    app_config: &Config,
    initial_prompt_text: Option<String>,
) -> Result<GeminiLiveClient<GeminiAppState>> {
    let gemini_app_state = GeminiAppState {
        current_turn_text: Arc::new(StdMutex::new(String::new())),
        turn_complete_signal: Arc::new(Notify::new()),
    };

    info!(
        "Configuring Gemini Live Client for model: {}",
        app_config.gemini_model_name
    );
    let mut builder = GeminiLiveClientBuilder::<GeminiAppState>::new_with_state(
        app_config.gemini_api_key.clone(),
        app_config.gemini_model_name.clone(),
        gemini_app_state.clone(),
    );

    // --- Audio Configuration ---
    // Assuming AudioConfig is in gemini_live_api::types
    // And builder has an .audio_config() method
    // If your crate uses a different struct name or method, adjust here.
    let audio_config = AudioConfig {
        // Ensure AudioConfig is imported from types::*
        sample_rate_hertz: Some(app_config.audio_sample_rate as i32), // API might expect i32
        audio_encoding: AudioEncoding::Linear16,                      // Commonly LINEAR16 for PCM
        // channels: Some(app_config.audio_channels as i32), // If your AudioConfig supports it
        ..Default::default() // If other fields are optional
    };
    builder = builder.audio_config(audio_config); // Corrected method name if needed

    builder = builder.generation_config(GenerationConfig {
        response_modalities: Some(vec![ResponseModality::Text]),
        temperature: Some(0.7),
        ..Default::default()
    });

    let system_message = initial_prompt_text.unwrap_or_else(||
        "You are a helpful voice assistant. Respond clearly and concisely. Use available tools when needed.".to_string()
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
    builder = builder.on_usage_metadata(handle_gemini_usage_metadata);
    builder = register_all_tools(builder);

    info!("Connecting Gemini client...");
    let client = builder.connect().await?;
    info!("Gemini client connected successfully.");
    Ok(client)
}
