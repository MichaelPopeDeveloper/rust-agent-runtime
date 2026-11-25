//! Direct Anthropic Messages API client.
//!
//! This is the path the original scaffold started on — one `reqwest` call to
//! the API — made correct: the real endpoint (`/v1/messages`), the `x-api-key`
//! and `anthropic-version` headers the API requires (not `Authorization`), a
//! configurable base URL, and errors that are propagated rather than dropped.
//! It is deliberately thin; for anything agentic, use the agent routes.

use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;

/// The API version header every Messages request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default model for [`MessagesClient::complete`] when none is given.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// A minimal Messages API client.
#[derive(Clone, Debug)]
pub struct MessagesClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl MessagesClient {
    /// A client for `base_url` (e.g. `https://api.anthropic.com`).
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        MessagesClient {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// `POST /v1/messages` with `body` as the request, returning the parsed
    /// response. A non-success status is returned as an [`ApiError`] carrying
    /// the upstream status and body so the client sees the real reason.
    pub async fn create(&self, body: &Value) -> Result<Value, ApiError> {
        let response = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            // Pass the upstream status through for client errors (a 400 from
            // the API is the caller's problem); anything else is a gateway
            // failure.
            let status = if status.is_client_error() {
                status
            } else {
                StatusCode::BAD_GATEWAY
            };
            return Err(ApiError::new(
                status,
                format!("Messages API returned {}: {}", status.as_u16(), text.trim()),
            ));
        }
        serde_json::from_str(&text)
            .map_err(|e| ApiError::upstream(format!("Messages API returned invalid JSON: {e}")))
    }

    /// Send a single user message and return the assistant's text — the
    /// scaffold's original "Hello, Claude!" use case, as a reusable call.
    pub async fn complete(
        &self,
        model: Option<&str>,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String, ApiError> {
        let body = json!({
            "model": model.unwrap_or(DEFAULT_MODEL),
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": prompt}],
        });
        let response = self.create(&body).await?;
        Ok(extract_text(&response))
    }
}

/// Concatenate the `text` content blocks of a Messages API response.
pub fn extract_text(response: &Value) -> String {
    response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_blocks_only() {
        let response = json!({
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "t", "name": "x", "input": {}},
                {"type": "text", "text": ", world"},
            ]
        });
        assert_eq!(extract_text(&response), "Hello, world");
        assert_eq!(extract_text(&json!({})), "");
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let client = MessagesClient::new("k", "https://example.test/");
        assert_eq!(client.base_url, "https://example.test");
    }
}
