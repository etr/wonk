//! Ollama embedding API client.
//!
//! Provides a synchronous HTTP client for generating text embeddings via
//! Ollama's `/api/embed` endpoint.  Supports health checking, batch
//! embedding, and configurable timeouts.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::Agent;

use crate::errors::EmbeddingError;

/// Default Ollama server URL.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Default embedding model.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";

// ---------------------------------------------------------------------------
// Serde types for the Ollama /api/embed endpoint
// ---------------------------------------------------------------------------

/// Request body for `POST /api/embed`.
#[derive(Serialize)]
pub(crate) struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
}

/// Response body from `POST /api/embed`.
#[derive(Deserialize)]
pub(crate) struct EmbedResponse {
    pub embeddings: Vec<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Synchronous HTTP client for Ollama's embedding API.
pub struct OllamaClient {
    agent: Agent,
    pub(crate) base_url: String,
    pub(crate) model: String,
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaClient {
    /// Create a client pointing at the default Ollama URL (`localhost:11434`).
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Create a client with a custom base URL.
    ///
    /// Configures connection timeout (2 s) and body-read timeout (60 s).
    /// Disables `http_status_as_error` so we can inspect non-200 responses
    /// ourselves.
    pub fn with_base_url(base_url: &str) -> Self {
        let config = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build();
        let agent: Agent = config.into();
        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Check whether the Ollama server is reachable.
    ///
    /// Sends `GET /` and returns `true` if the server responds with 200 OK.
    pub fn is_healthy(&self) -> bool {
        let url = format!("{}/", self.base_url);
        match self.agent.get(&url).call() {
            Ok(resp) => resp.status() == 200,
            Err(_) => false,
        }
    }

    /// Generate embeddings for a batch of texts.
    ///
    /// Returns one `Vec<f32>` per input string.  An empty input slice
    /// short-circuits to an empty result without contacting the server.
    pub fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/api/embed", self.base_url);
        let request_body = EmbedRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let response = self
            .agent
            .post(&url)
            .send_json(&request_body)
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if status != 200 {
            let body = response.into_body().read_to_string().unwrap_or_default();
            return Err(EmbeddingError::OllamaError(extract_error_detail(
                status, &body,
            )));
        }

        let embed_resp: EmbedResponse = response
            .into_body()
            .read_json()
            .map_err(|_| EmbeddingError::InvalidResponse)?;

        Ok(embed_resp.embeddings)
    }

    /// Generate an embedding for a single text.
    ///
    /// Convenience wrapper around [`embed_batch`](Self::embed_batch).
    pub fn embed_single(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut results = self.embed_batch(&[text.to_string()])?;
        results.pop().ok_or(EmbeddingError::InvalidResponse)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a ureq transport error to the appropriate [`EmbeddingError`].
///
/// Connection-level failures (refused, host not found, timeout) become
/// [`EmbeddingError::OllamaUnreachable`].  Everything else is wrapped as
/// [`EmbeddingError::OllamaError`].
fn classify_error(err: ureq::Error) -> EmbeddingError {
    match err {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(_) => {
            EmbeddingError::OllamaUnreachable
        }
        ureq::Error::Io(ref io_err)
            if matches!(
                io_err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            EmbeddingError::OllamaUnreachable
        }
        other => EmbeddingError::OllamaError(other.to_string()),
    }
}

/// Try to extract a human-readable message from an Ollama error response body.
///
/// Ollama returns `{"error":"..."}` on failure.  If the body cannot be parsed,
/// falls back to `"HTTP {status}"`.
fn extract_error_detail(status: u16, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(msg) = json.get("error").and_then(|v| v.as_str())
    {
        return msg.to_string();
    }
    format!("HTTP {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_client_uses_localhost() {
        let client = OllamaClient::new();
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert_eq!(client.model, DEFAULT_MODEL);
    }

    #[test]
    fn with_base_url_trims_trailing_slash() {
        let client = OllamaClient::with_base_url("http://example.com:11434/");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    #[test]
    fn with_base_url_preserves_clean_url() {
        let client = OllamaClient::with_base_url("http://example.com:11434");
        assert_eq!(client.base_url, "http://example.com:11434");
    }

    // -- Health check tests ---------------------------------------------------

    #[test]
    fn health_check_returns_false_when_unreachable() {
        // Port 19999 should have nothing listening.
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        assert!(!client.is_healthy());
    }

    // -- Connection error classification tests --------------------------------

    #[test]
    fn classify_error_connection_refused_is_unreachable() {
        let err = ureq::Error::ConnectionFailed;
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_host_not_found_is_unreachable() {
        let err = ureq::Error::HostNotFound;
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_timeout_is_unreachable() {
        let err = ureq::Error::Timeout(ureq::Timeout::Connect);
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_io_connection_refused_is_unreachable() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = ureq::Error::Io(io_err);
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaUnreachable));
    }

    #[test]
    fn classify_error_other_is_ollama_error() {
        let err = ureq::Error::BadUri("bad".into());
        let result = classify_error(err);
        assert!(matches!(result, EmbeddingError::OllamaError(_)));
    }

    // -- Error detail extraction tests ----------------------------------------

    #[test]
    fn extract_error_detail_parses_json_error_field() {
        let body = r#"{"error":"model not found"}"#;
        assert_eq!(extract_error_detail(400, body), "model not found");
    }

    #[test]
    fn extract_error_detail_falls_back_to_status() {
        assert_eq!(extract_error_detail(500, "not json"), "HTTP 500");
    }

    #[test]
    fn extract_error_detail_falls_back_on_missing_field() {
        let body = r#"{"status":"bad"}"#;
        assert_eq!(extract_error_detail(422, body), "HTTP 422");
    }

    // -- embed_batch tests ----------------------------------------------------

    #[test]
    fn embed_batch_empty_returns_empty_vec() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_batch(&[]);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn embed_batch_unreachable_returns_error() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let texts = vec!["hello".to_string()];
        let result = client.embed_batch(&texts);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EmbeddingError::OllamaUnreachable
        ));
    }

    // -- embed_single tests ---------------------------------------------------

    #[test]
    fn embed_single_unreachable_returns_error() {
        let client = OllamaClient::with_base_url("http://127.0.0.1:19999");
        let result = client.embed_single("hello");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            EmbeddingError::OllamaUnreachable
        ));
    }

    // -- Serde round-trip tests -----------------------------------------------

    #[test]
    fn embed_request_serializes_correctly() {
        let req = EmbedRequest {
            model: "test-model".to_string(),
            input: vec!["hello".to_string(), "world".to_string()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["input"][0], "hello");
        assert_eq!(json["input"][1], "world");
    }

    #[test]
    fn embed_response_deserializes_correctly() {
        let json = r#"{"embeddings":[[0.1,0.2,0.3],[0.4,0.5,0.6]]}"#;
        let resp: EmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.embeddings.len(), 2);
        assert_eq!(resp.embeddings[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(resp.embeddings[1], vec![0.4, 0.5, 0.6]);
    }
}
