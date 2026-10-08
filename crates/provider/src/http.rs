//! Shared HTTP client for provider calls.

use reqwest::blocking::Client;
use std::time::Duration;

/// A single model turn can stream for minutes. `reqwest::blocking::Client::new()`
/// defaults to a 30 s *total* timeout, which truncates those responses with
/// "error decoding response body". Mirrors the 10-minute budget the official
/// Anthropic SDK uses.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

pub fn client() -> Client {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new())
}
