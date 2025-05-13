// src/config.rs
use anyhow::{Context, Result};
use std::env;

pub struct Config {
    pub openai_api_key: String,
    pub openai_model_name: String,
    pub audio_sample_rate: u32, // This will now be the rate for local mic/TCP input
    pub audio_channels: u16,
    pub audio_source: String,
    pub tcp_server_address: Option<String>, // For TCP audio input from ESP32
    pub audio_output_type: String,
    pub speaker_tcp_output_address: Option<String>, // For TCP audio output to ESP32
    pub backend_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let openai_api_key =
            env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set in .env file")?;
        let openai_model_name = env::var("OPENAI_MODEL_NAME")
            .unwrap_or_else(|_| "gpt-4o-mini-realtime-preview".to_string());

        // If ESP32 is the source, it dictates 24kHz.
        // If local mic is source, it might be different, but OpenAI needs 24kHz.
        // Let's make the default 24000Hz for consistency with OpenAI and ESP32 setup.
        let audio_sample_rate = env::var("AUDIO_SAMPLE_RATE")
            .unwrap_or_else(|_| "24000".to_string()) // Default to 24kHz
            .parse::<u32>()
            .context("Invalid AUDIO_SAMPLE_RATE")?;

        let audio_channels = env::var("AUDIO_CHANNELS")
            .unwrap_or_else(|_| "1".to_string())
            .parse::<u16>()
            .context("Invalid AUDIO_CHANNELS")?;

        let audio_source = env::var("AUDIO_SOURCE").unwrap_or_else(|_| "MIC".to_string());
        let tcp_server_address = env::var("TCP_SERVER_ADDRESS").ok(); // ESP32's Mic Server Address

        let audio_output_type =
            env::var("AUDIO_OUTPUT_TYPE").unwrap_or_else(|_| "SPEAKER".to_string());
        // Renamed for clarity: this is the address of the ESP32's speaker TCP server
        let speaker_tcp_output_address = env::var("SPEAKER_TCP_TARGET_ADDRESS").ok();

        let backend_url = env::var("BACKEND_URL").ok();

        if audio_source.to_uppercase() == "TCP" && tcp_server_address.is_none() {
            return Err(anyhow::anyhow!(
                "TCP_SERVER_ADDRESS (for ESP32 mic input) must be set if AUDIO_SOURCE is TCP."
            ));
        }
        if audio_output_type.to_uppercase() == "TCP_SPEAKER" && speaker_tcp_output_address.is_none()
        {
            return Err(anyhow::anyhow!(
                "SPEAKER_TCP_TARGET_ADDRESS (for ESP32 speaker output) must be set if AUDIO_OUTPUT_TYPE is TCP_SPEAKER."
            ));
        }

        if audio_channels != 1 {
            tracing::warn!(
                "AUDIO_CHANNELS is set to {}. This application works best with mono (1) audio.",
                audio_channels
            );
        }
        // OpenAI "pcm16" input requires 24kHz.
        // If AUDIO_SOURCE is "MIC" (local), the `MicAudioInput` will try to get `audio_sample_rate`.
        // If `audio_sample_rate` is not 24000, you'd need resampling before sending to OpenAI.
        // For simplicity, this setup now assumes AUDIO_SAMPLE_RATE is 24000 if using local mic with OpenAI.
        if audio_sample_rate != 24000 {
            tracing::warn!(
                "AUDIO_SAMPLE_RATE is configured to {}. OpenAI's 'pcm16' format expects 24000 Hz. \
                 If using local MIC, ensure it can provide 24kHz or implement resampling. \
                 If AUDIO_SOURCE=TCP (ESP32), the ESP32 must send 24kHz.",
                audio_sample_rate
            );
        }

        Ok(Self {
            openai_api_key,
            openai_model_name,
            audio_sample_rate, // This is the rate of audio from the source (local mic or ESP32 TCP mic)
            audio_channels,
            audio_source,
            tcp_server_address,
            audio_output_type,
            speaker_tcp_output_address,
            backend_url,
        })
    }
}
