use anyhow::{Context, Result};
use std::env;

pub struct Config {
    pub gemini_api_key: String,
    pub gemini_model_name: String,
    pub audio_sample_rate: u32,
    pub audio_channels: u16,
    pub audio_source: String,
    pub tcp_server_address: Option<String>,
    pub audio_output_type: String,
    pub udp_output_address: Option<String>,
    pub openweathermap_api_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let gemini_api_key =
            env::var("GEMINI_API_KEY").context("GEMINI_API_KEY not set in .env file")?;
        let gemini_model_name = env::var("GEMINI_MODEL_NAME")
            .unwrap_or_else(|_| "models/gemini-1.5-flash-latest".to_string());

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
        let udp_output_address = env::var("UDP_OUTPUT_ADDRESS").ok();

        let openweathermap_api_key = env::var("OPENWEATHERMAP_API_KEY").ok();

        if audio_source.to_uppercase() == "TCP" && tcp_server_address.is_none() {
            return Err(anyhow::anyhow!(
                "TCP_SERVER_ADDRESS must be set if AUDIO_SOURCE is TCP."
            ));
        }
        if audio_output_type.to_uppercase() == "UDP" && udp_output_address.is_none() {
            return Err(anyhow::anyhow!(
                "UDP_OUTPUT_ADDRESS must be set if AUDIO_OUTPUT_TYPE is UDP."
            ));
        }

        if audio_channels != 1 {
            tracing::warn!(
                "AUDIO_CHANNELS is set to {}. This application works best with mono (1) audio for both input and output.",
                audio_channels
            );
        }
        if audio_sample_rate != 16000 {
            tracing::warn!(
                "AUDIO_SAMPLE_RATE is set to {}. 16000 Hz is recommended for voice applications.",
                audio_sample_rate
            );
        }

        Ok(Self {
            gemini_api_key,
            gemini_model_name,
            audio_sample_rate,
            audio_channels,
            audio_source,
            tcp_server_address,
            audio_output_type,
            udp_output_address,
            openweathermap_api_key,
        })
    }
}
