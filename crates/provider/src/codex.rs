//! `openai-codex` provider — ChatGPT subscription via the Responses wire.
//!
//! Thin wrapper around [`crate::openai_responses::OpenAiResponsesClient`] with the
//! Codex backend base URL. Full OAuth refresh / plan-limit handling remains in the
//! Python `codex_provider` path; Rust uses the profile bearer (or api_key) as-is.

use crate::openai_responses::OpenAiResponsesClient;

pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Codex client = Responses client pointed at the subscription backend.
pub type CodexClient = OpenAiResponsesClient;

/// Build a Responses client for the Codex backend.
pub fn new_client(base_url: Option<String>, api_key: String, default_model: String) -> CodexClient {
    let base = base_url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| CODEX_BASE_URL.to_string());
    OpenAiResponsesClient::new(base, api_key, default_model).with_name("openai-codex")
}
