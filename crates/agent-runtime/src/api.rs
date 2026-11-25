//! Direct Anthropic Messages API client.
//!
//! This is the path the original scaffold started on — one `reqwest` call to
//! the API — made correct: the real endpoint (`/v1/messages`), the `x-api-key`
//! and `anthropic-version` headers the API requires (not `Authorization`), a
//! configurable base URL, and errors that are propagated rather than dropped.
//! It is deliberately thin; for anything agentic, use the agent routes.
//!
//! Transient failures are retried: `429` (rate limited), `529` (overloaded)
//! and other `5xx`/connection errors get a couple of attempts with backoff,
//! honouring `retry-after` when the API sends one. A single `/v1/messages`
//! call is idempotent from the caller's point of view, so this is safe.

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;

/// The API version header every Messages request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default model for [`MessagesClient::complete`] when none is given.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// Retries after the first attempt on a transient failure.
pub const MAX_RETRIES: u32 = 2;

/// Backoff before the first retry (doubles each time) when the API sends no
/// `retry-after`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Longest the client will wait on a `retry-after` before giving up instead.
const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// A minimal Messages API client.
#[derive(Clone, Debug)]
pub struct MessagesClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    max_retries: u32,
}

impl MessagesClient {
    /// A client for `base_url` (e.g. `https://api.anthropic.com`).
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            // A hung upstream must not hang a handler forever. The read
            // timeout is between bytes, so a long streaming response is fine
            // while a dead connection is noticed; ten minutes matches the
            // official SDKs' default for a slow non-streaming completion.
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_default();
        MessagesClient {
            http,
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            max_retries: MAX_RETRIES,
        }
    }

    /// Change how many times a transient failure is retried (`0` disables).
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// `POST /v1/messages` with `body` as the request, returning the raw
    /// response once a success status arrives, retrying transient failures
    /// first (see the module docs). The body is untouched, so a caller can
    /// relay a streaming response as it arrives; [`create`](Self::create)
    /// reads and parses it instead.
    pub async fn send(&self, body: &Value) -> Result<reqwest::Response, ApiError> {
        let mut backoff = INITIAL_BACKOFF;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (error, retry_after) = match self.attempt(body).await {
                Ok(response) => return Ok(response),
                Err(Attempt::Final(error)) => return Err(error),
                Err(Attempt::Transient { error, retry_after }) => (error, retry_after),
            };
            if attempt > self.max_retries {
                return Err(error);
            }
            let wait = retry_after.unwrap_or(backoff);
            if wait > MAX_BACKOFF {
                return Err(error);
            }
            tracing::warn!(attempt, wait_ms = wait.as_millis() as u64, error = %error.message, "retrying Messages API call");
            tokio::time::sleep(wait).await;
            backoff *= 2;
        }
    }

    /// `POST /v1/messages` with `body` as the request, returning the parsed
    /// response. A non-success status is returned as an [`ApiError`] carrying
    /// the upstream status and body so the client sees the real reason.
    pub async fn create(&self, body: &Value) -> Result<Value, ApiError> {
        let text = self.send(body).await?.text().await?;
        serde_json::from_str(&text)
            .map_err(|e| ApiError::upstream(format!("Messages API returned invalid JSON: {e}")))
    }

    async fn attempt(&self, body: &Value) -> Result<reqwest::Response, Attempt> {
        let response = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| Attempt::Transient {
                error: ApiError::from(e),
                retry_after: None,
            })?;

        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = parse_retry_after(response.headers());
        let text = response.text().await.unwrap_or_default();
        // Pass the upstream status through for client errors (a 400 from the
        // API is the caller's problem); anything else is a gateway failure.
        let reported = if status.is_client_error() {
            status
        } else {
            StatusCode::BAD_GATEWAY
        };
        let error = ApiError::new(
            reported,
            format!("Messages API returned {}: {}", status.as_u16(), text.trim()),
        );
        if is_transient(status) {
            Err(Attempt::Transient { error, retry_after })
        } else {
            Err(Attempt::Final(error))
        }
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

/// One attempt's failure, and whether another attempt could succeed.
enum Attempt {
    Final(ApiError),
    Transient {
        error: ApiError,
        retry_after: Option<Duration>,
    },
}

/// Statuses worth retrying: rate limits, overload, request timeouts, lock
/// conflicts and server-side failures. The Anthropic SDKs retry the same set.
fn is_transient(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::CONFLICT | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

/// `retry-after` in seconds, when present and sane. (The HTTP-date form is
/// rare from the API and falls back to the client's own backoff.)
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|secs| *secs >= 0.0)
        .map(Duration::from_secs_f64)
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

    #[test]
    fn transient_statuses() {
        assert!(is_transient(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient(StatusCode::from_u16(529).unwrap()));
        assert!(is_transient(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_transient(StatusCode::BAD_REQUEST));
        assert!(!is_transient(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn retry_after_seconds_are_parsed() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }
}
