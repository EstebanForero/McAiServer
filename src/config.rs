// src/config.rs
use anyhow::{Context, Result};
use std::env;

pub struct Config {
    pub openai_api_key: String,    // <<< CHANGED
    pub openai_model_name: String, // <<< CHANGED
    pub audio_sample_rate: u32,
    pub audio_channels: u16,
    pub audio_source: String,
    pub tcp_server_address: Option<String>,
    pub audio_output_type: String,
    pub udp_output_address: Option<String>, // Kept if you still use it for TCP_SPEAKER example
    pub backend_url: Option<String>, // Likely not used for OpenAI directly, but kept for tools
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let openai_api_key = // <<< CHANGED
            env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set in .env file")?;
        let openai_model_name =
            env::var("OPENAI_MODEL_NAME") // <<< CHANGED
                .unwrap_or_else(|_| "gpt-4o-mini-realtime-preview".to_string()); // Default to realtime model

        let audio_sample_rate = env::var("AUDIO_SAMPLE_RATE")
            .unwrap_or_else(|_| "16000".to_string())
            .parse::<u32>()
            .context("Invalid AUDIO_SAMPLE_RATE")?;
        let audio_channels = env::var("AUDIO_CHANNELS")
            .unwrap_or_else(|_| "1".to_string())
            .parse::<u16>()
            .context("Invalid AUDIO_CHANNELS")?;

        let audio_source = env::var("AUDIO_SOURCE").unwrap_or_else(|_| "MIC".to_string());
        let tcp_server_address = env::var("TCP_SERVER_ADDRESS").ok();

        let audio_output_type =
            env::var("AUDIO_OUTPUT_TYPE").unwrap_or_else(|_| "SPEAKER".to_string());
        let udp_output_address = env::var("UDP_OUTPUT_ADDRESS").ok(); // For ESP32 TCP Speaker example

        let backend_url = env::var("BACKEND_URL").ok(); // For tools that call an external backend

        if audio_source.to_uppercase() == "TCP" && tcp_server_address.is_none() {
            return Err(anyhow::anyhow!(
                "TCP_SERVER_ADDRESS must be set if AUDIO_SOURCE is TCP."
            ));
        }
        if audio_output_type.to_uppercase() == "TCP_SPEAKER" && udp_output_address.is_none() {
            // Corrected logic for UDP_OUTPUT_ADDRESS as target
            return Err(anyhow::anyhow!(
                "UDP_OUTPUT_ADDRESS (acting as TARGET_ADDRESS for TCP_SPEAKER) must be set if AUDIO_OUTPUT_TYPE is TCP_SPEAKER."
            ));
        }

        if audio_channels != 1 {
            tracing::warn!(
                "AUDIO_CHANNELS is set to {}. This application works best with mono (1) audio for both input and output.",
                audio_channels
            );
        }
        // Input sample rate for OpenAI should be 16000 Hz usually.
        // Output sample rate from OpenAI is typically 24000 Hz.
        // The config here is for INPUT.

        Ok(Self {
            openai_api_key,
            openai_model_name,
            audio_sample_rate,
            audio_channels,
            audio_source,
            tcp_server_address,
            audio_output_type,
            udp_output_address,
            backend_url,
        })
    }
}
