use gemini_live_api::{GeminiLiveClientBuilder, tool_function};
use reqwest::{Method, Url};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{error, info};

use super::GeminiAppState;

#[tool_function("Fetches the current weather for a specified city. Uses OpenWeatherMap.")]
async fn get_weather(state: Arc<GeminiAppState>, city: String) -> Result<String, String> {
    let response = reqwest::get(&state.backend_url).await.map_err(|err| {
        error!("Error calling backend endpoint: {}", err);
        err.to_string()
    })?;

    let text_response = response.text().await.map_err(|err| {
        error!("Error converting response to text: {err}");
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function("Post example")]
async fn post_example(state: Arc<GeminiAppState>, city: String) -> Result<String, String> {
    let client = reqwest::Client::new();

    #[derive(Serialize)]
    struct City {
        city_name: String,
    }

    let res = client
        .post(&state.backend_url)
        .json(&City { city_name: city })
        .send()
        .await
        .map_err(|err| {
            error!("Error sending post example: {err}");
            err.to_string()
        })?;

    let text = res.text().await.map_err(|err| {
        error!("Error converting response to text: {err}");
        err.to_string()
    })?;

    Ok(text)
}

#[tool_function("Echoes back the provided text. Useful for testing.")]
async fn echo_message(_state: Arc<GeminiAppState>, message: String) -> String {
    println!("[Tool] 'echo_message' called with: \"{}\"", message);
    format!("Echo: \"{}\"", message)
}

pub fn register_all_tools(
    builder: GeminiLiveClientBuilder<GeminiAppState>,
) -> GeminiLiveClientBuilder<GeminiAppState> {
    let builder = get_weather_register_tool(builder);
    let builder = echo_message_register_tool(builder);
    info!("Registered tools: get_weather, echo_message");
    builder
}
