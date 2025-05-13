use std::sync::Arc;

use gemini_live_api::{AiClientBuilder, tool_function};
use serde::Serialize;
use tracing::{error, info, warn};

use super::OpenAiAppState;

#[tool_function(
    "Deletes a specific dish from a given order. Requires the order ID and the dish ID."
)]
async fn delete_invoice_dish(
    state: Arc<OpenAiAppState>,
    order_id: u32,
    dish_id: u32,
) -> Result<String, String> {
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(order_id, dish_id, "Attempting to delete dish from order");
    let client = reqwest::Client::new();
    let url = format!("{}/invoice/dish/{}/{}", backend_url, order_id, dish_id);
    info!(%url, "Sending DELETE request to delete dish");

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

    info!(order_id, dish_id, %url, "Successfully deleted dish from order");
    Ok(text_response)
}

#[tool_function(
    "Updates the status of a given order to 'Pending' (for preparation). Requires the order ID."
)]
async fn set_order_status_to_pending(
    state: Arc<OpenAiAppState>, // <<< Make sure this matches
    order_id: u32,
) -> Result<String, String> {
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(order_id, "Attempting to set order status to Pending");
    let client = reqwest::Client::new();
    let url = format!("{}/changestatus/{}/Pending", backend_url, order_id);
    info!(%url, "Sending PUT request to set order status");

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

    info!(order_id, %url, "Successfully set order status to Pending");
    Ok(text_response)
}

#[tool_function("Retrieves the total amount for a specific order. Requires the order ID.")]
async fn get_order_total(state: Arc<OpenAiAppState>, order_id: u32) -> Result<String, String> {
    // <<< Make sure this matches
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(order_id, "Attempting to get order total");
    let url = format!("{}/total/{}", backend_url, order_id);
    info!(%url, "Sending GET request for order total");

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

    info!(order_id, %url, "Successfully retrieved order total");
    warn!(text_response);
    Ok(text_response)
}

#[tool_function("Fetches all dishes associated with a specific order. Requires the order ID.")]
async fn get_order_dishes(state: Arc<OpenAiAppState>, order_id: u32) -> Result<String, String> {
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(order_id, "Attempting to get dishes for order");
    let url = format!("{}/dishes/{}", backend_url, order_id);
    info!(%url, "Sending GET request for order dishes");

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

    warn!(order_id, %url, "Successfully retrieved dishes for order: {text_response}");
    Ok(text_response)
}

#[tool_function("Retrieves a list of all available dishes in the restaurant.")]
async fn get_all_dishes(state: Arc<OpenAiAppState>) -> Result<String, String> {
    // <<< Make sure this matches
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!("Attempting to get all dishes");
    let url = format!("{}/dishes", backend_url);
    info!(%url, "Sending GET request for all dishes");

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

    warn!(%url, "Successfully retrieved all dishes: {text_response}");
    Ok(text_response)
}

#[tool_function(
    "Creates a new order for a specified table. The order is initialized with 'Loading' status. Requires the table number and returns the new order ID or details."
)]
async fn create_order(state: Arc<OpenAiAppState>, table_number: u32) -> Result<String, String> {
    // <<< Make sure this matches
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(
        table_number,
        "Attempting to create a new order, with table number: {table_number}"
    );
    let client = reqwest::Client::new();
    let url = format!("{}/order", backend_url);

    #[derive(Serialize, Debug)]
    struct NewOrderPayload {
        id_desk: u32,
        status: String,
    }

    let payload = NewOrderPayload {
        id_desk: table_number,
        status: "Loading".to_string(),
    };
    info!(%url, payload = ?payload, "Sending POST request to create order");

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

    info!(table_number, %url, "Successfully created new order: {text_response}");
    Ok(text_response)
}

#[tool_function(
    "Adds a specific dish to an existing order. Requires the order ID and the dish ID."
)]
async fn add_dish_to_order(
    state: Arc<OpenAiAppState>, // <<< Make sure this matches
    order_id: u32,
    dish_id: u32,
) -> Result<String, String> {
    let backend_url = state
        .backend_url
        .as_ref()
        .ok_or_else(|| "Backend URL not configured".to_string())?;
    warn!(order_id, dish_id, "Attempting to add dish to order");
    let client = reqwest::Client::new();
    let url = format!("{}/invoice", backend_url);

    #[derive(Serialize, Debug)]
    struct AddToInvoicePayload {
        id_order: u32,
        id_dish: u32,
    }

    let payload = AddToInvoicePayload {
        id_dish: dish_id,
        id_order: order_id,
    };
    info!(%url, payload = ?payload, "Sending POST request to add dish to order");

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

    warn!(order_id, dish_id, %url, "Successfully added dish to order: {text_response}");
    Ok(text_response)
}

// Adapt the register_all_tools function
pub fn register_all_tools(
    builder: AiClientBuilder<OpenAiAppState>, // <<< CHANGED
) -> AiClientBuilder<OpenAiAppState> {
    // <<< CHANGED
    let builder = get_all_dishes_register_tool(builder);
    let builder = add_dish_to_order_register_tool(builder);
    let builder = create_order_register_tool(builder);
    let builder = get_order_dishes_register_tool(builder);
    let builder = get_order_total_register_tool(builder);
    let builder = set_order_status_to_pending_register_tool(builder);
    let builder = delete_invoice_dish_register_tool(builder);
    info!("Registered all McDonald's tools for OpenAI."); // <<< CHANGED
    builder
}
