use gemini_live_api::{GeminiLiveClientBuilder, tool_function};
use reqwest::{Method, Url};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{error, info};

use super::GeminiAppState;

#[tool_function(
    "Deletes a specific dish from a given order. Requires the order ID and the dish ID."
)]
async fn delete_invoice_dish(
    state: Arc<GeminiAppState>,
    order_id: u32,
    dish_id: u32,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/invoice/dish/{}/{}",
        state.backend_url, order_id, dish_id
    );

    let response = client.delete(&url).send().await.map_err(|err| {
        error!("Error sending DELETE request to {}: {}", url, err);
        err.to_string()
    })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error deleting dish from invoice: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!(
            "Error converting delete_invoice_dish response to text: {}",
            err
        );
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function(
    "Updates the status of a given order to 'Pending' (for preparation). Requires the order ID."
)]
async fn set_order_status_to_pending(
    state: Arc<GeminiAppState>,
    order_id: u32,
) -> Result<String, String> {
    let client = reqwest::Client::new();

    let url = format!("{}/changestatus/{}/Pending", state.backend_url, order_id);

    let response = client.put(&url).send().await.map_err(|err| {
        error!("Error sending PUT request to {}: {}", url, err);
        err.to_string()
    })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error setting order status to pending: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!(
            "Error converting set_order_status_to_pending response to text: {}",
            err
        );
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function("Retrieves the total amount for a specific order. Requires the order ID.")]
async fn get_order_total(state: Arc<GeminiAppState>, order_id: u32) -> Result<String, String> {
    let url = format!("{}/total/{}", state.backend_url, order_id);

    let response = reqwest::get(&url).await.map_err(|err| {
        error!("Error sending GET request to {}: {}", url, err);
        err.to_string()
    })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error getting order total: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!("Error converting get_order_total response to text: {}", err);
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function("Fetches all dishes associated with a specific order. Requires the order ID.")]
async fn get_order_dishes(state: Arc<GeminiAppState>, order_id: u32) -> Result<String, String> {
    let url = format!("{}/dishes/{}", state.backend_url, order_id);

    let response = reqwest::get(&url).await.map_err(|err| {
        error!("Error sending GET request to {}: {}", url, err);
        err.to_string()
    })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error getting invoice dishes: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!(
            "Error converting get_invoice_dishes response to text: {}",
            err
        );
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function("Retrieves a list of all available dishes in the restaurant.")]
async fn get_all_dishes(state: Arc<GeminiAppState>) -> Result<String, String> {
    let url = format!("{}/dishes", state.backend_url);

    let response = reqwest::get(&url).await.map_err(|err| {
        error!("Error sending GET request to {}: {}", url, err);
        err.to_string()
    })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error getting all dishes: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!("Error converting get_all_dishes response to text: {}", err);
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function(
    "Creates a new order for a specified table. The order is initialized with 'Loading' status. Requires the table number and returns the new order ID or details."
)]
async fn create_order(state: Arc<GeminiAppState>, table_number: u32) -> Result<String, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/order", state.backend_url);

    #[derive(Serialize)]
    struct NewOrderPayload {
        id_desk: u32,
        status: String,
    }

    let payload = NewOrderPayload {
        id_desk: table_number,
        status: "Loading".to_string(),
    };

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            error!("Error sending POST request to {}: {}", url, err);
            err.to_string()
        })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error creating order: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!("Error converting create_order response to text: {}", err);
        err.to_string()
    })?;

    Ok(text_response)
}

#[tool_function(
    "Adds a specific dish to an existing order. Requires the order ID and the dish ID."
)]
async fn add_dish_to_order(
    state: Arc<GeminiAppState>,
    order_id: u32,
    dish_id: u32,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/invoice", state.backend_url);

    #[derive(Serialize)]
    struct AddToInvoicePayload {
        id_order: u32,
        id_dish: u32,
    }

    let payload = AddToInvoicePayload {
        id_dish: dish_id,
        id_order: order_id,
    };

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            error!("Error sending POST request to {}: {}", url, err);
            err.to_string()
        })?;

    if !response.status().is_success() {
        let error_message = format!(
            "Error adding dish to invoice: {} - {}",
            response.status(),
            response
                .text()
                .await
                .unwrap_or_else(|_| "Could not retrieve error body".to_string())
        );
        error!("{}", error_message);
        return Err(error_message);
    }

    let text_response = response.text().await.map_err(|err| {
        error!(
            "Error converting add_dish_to_invoice response to text: {}",
            err
        );
        err.to_string()
    })?;

    Ok(text_response)
}

pub fn register_all_tools(
    builder: GeminiLiveClientBuilder<GeminiAppState>,
) -> GeminiLiveClientBuilder<GeminiAppState> {
    let builder = get_all_dishes_register_tool(builder);
    let builder = add_dish_to_order_register_tool(builder);
    let builder = create_order_register_tool(builder);
    let builder = get_all_dishes_register_tool(builder);
    let builder = get_order_dishes_register_tool(builder);
    let builder = get_order_total_register_tool(builder);
    let builder = set_order_status_to_pending_register_tool(builder);
    let builder = delete_invoice_dish_register_tool(builder);
    info!("Registered tools: get_weather, echo_message");
    builder
}
