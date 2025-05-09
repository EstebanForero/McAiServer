use gemini_live_api::{GeminiLiveClientBuilder, tool_function};
use reqwest;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, warn};

use super::GeminiAppState;

#[derive(Deserialize, Debug)]
struct WeatherApiResponse {
    main: Option<WeatherMain>,
    weather: Option<Vec<WeatherDescription>>,
    name: Option<String>,
    cod: Option<serde_json::Value>,
    message: Option<String>,
}
#[derive(Deserialize, Debug)]
struct WeatherMain {
    temp: f32,
    feels_like: f32,
    humidity: u32,
}
#[derive(Deserialize, Debug)]
struct WeatherDescription {
    description: String,
}

#[tool_function("Fetches the current weather for a specified city. Uses OpenWeatherMap.")]
async fn get_weather(_state: Arc<GeminiAppState>, city: String) -> Result<String, String> {
    info!("[Tool] 'get_weather' called for city: {}", city);

    let openweathermap_api_key = std::env::var("OPENWEATHERMAP_API_KEY").ok();

    if openweathermap_api_key.is_none()
        || openweathermap_api_key.as_deref() == Some("")
        || openweathermap_api_key.as_deref() == Some("YOUR_OPENWEATHERMAP_API_KEY")
    {
        warn!(
            "[Tool] OPENWEATHERMAP_API_KEY not set or is placeholder. Mocking weather response for '{}'.",
            city
        );
        return Ok(format!(
            "Mock weather for {}: Sunny, 22°C. Please set a real OPENWEATHERMAP_API_KEY for live data.",
            city
        ));
    }
    let api_key = openweathermap_api_key.unwrap();

    let url = format!(
        "https://api.openweathermap.org/data/2.5/weather?q={}&appid={}&units=metric",
        city, api_key
    );

    match reqwest::get(&url).await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                match response.json::<WeatherApiResponse>().await {
                    Ok(data) => {
                        if data.cod.is_some()
                            && data
                                .cod
                                .as_ref()
                                .unwrap_or(&serde_json::Value::Null)
                                .as_str()
                                == Some("404")
                        {
                            let err_msg = format!(
                                "City '{}' not found by weather API. Message: {:?}",
                                city,
                                data.message.unwrap_or_default()
                            );
                            error!("[Tool Error] {}", err_msg);
                            return Err(err_msg);
                        }

                        let city_name = data.name.unwrap_or_else(|| city.clone());
                        let temp = data
                            .main
                            .as_ref()
                            .map_or("N/A".to_string(), |m| format!("{:.1}", m.temp));
                        let feels_like = data
                            .main
                            .as_ref()
                            .map_or("N/A".to_string(), |m| format!("{:.1}", m.feels_like));
                        let humidity = data
                            .main
                            .as_ref()
                            .map_or("N/A".to_string(), |m| m.humidity.to_string());
                        let description = data
                            .weather
                            .as_ref()
                            .and_then(|w| w.first())
                            .map_or("N/A", |d| &d.description);

                        Ok(format!(
                            "Current weather in {}: Temperature is {}°C (feels like {}°C), with {}% humidity. Conditions: {}.",
                            city_name, temp, feels_like, humidity, description
                        ))
                    }
                    Err(e) => {
                        let err_msg = format!(
                            "Failed to parse weather JSON response for '{}': {}",
                            city, e
                        );
                        error!("[Tool Error] {}", err_msg);
                        Err(err_msg)
                    }
                }
            } else {
                let err_msg = format!(
                    "Weather API request for '{}' failed with status: {}. Body: {:?}",
                    city,
                    status,
                    response.text().await.unwrap_or_default()
                );
                error!("[Tool Error] {}", err_msg);
                Err(err_msg)
            }
        }
        Err(e) => {
            let err_msg = format!("Network error fetching weather for '{}': {}", city, e);
            error!("[Tool Error] {}", err_msg);
            Err(err_msg)
        }
    }
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
