//! Error types for the provider layer.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error: {code} — {message}")]
    Api { code: String, message: String },

    #[error("Missing API key for {provider}")]
    MissingKey { provider: String },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Token limit exceeded: {0}")]
    ContextLimit(String),

    #[error("Unsupported model: {0}")]
    UnsupportedModel(String),

    #[error("Request failed: {0}")]
    RequestFailed(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Classify an HTTP status code + body into an Error variant.
    pub fn from_response(status: u16, body: &str, provider: &str) -> Self {
        // Try to parse structured error
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(msg) = v
                .get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
            {
                let code = v
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .or_else(|| v.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("api_error")
                    .to_string();
                return Self::Api {
                    code,
                    message: msg.to_string(),
                };
            }
        }
        if status == 401 || status == 403 {
            return Self::MissingKey {
                provider: provider.to_string(),
            };
        }
        if status == 429 {
            return Self::Other("Rate limit exceeded".to_string());
        }
        // Clip the body on a char boundary — error payloads can carry CJK text,
        // and a byte-offset slice mid-sequence panics.
        let mut end = body.len().min(200);
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        Self::Other(format!("HTTP {status}: {}", &body[..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_response_clips_cjk_body_without_panicking() {
        // Regression: `&body[..body.len().min(200)]` sliced mid-sequence on
        // multi-byte error payloads (200 is not a boundary of 3-byte CJK chars).
        let body = "错".repeat(100); // 300 bytes
        let err = Error::from_response(500, &body, "test");
        match err {
            Error::Other(msg) => {
                assert!(msg.starts_with("HTTP 500: "));
                assert!(msg.len() <= "HTTP 500: ".len() + 200);
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
