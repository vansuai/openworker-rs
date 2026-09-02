//! OpenWorker Provider Layer — Rust reimplementation of `coworker/providers/`.
//!
//! Plugs into the Rust (or PyO3-wrapped) turn engine via a clean `Provider` trait.
//! Vendors supported:
//! - OpenAI-compatible (OpenAI SDK shape, /v1/chat/completions) — OpenAI, Ollama, OpenRouter, etc.
//! - Anthropic Messages API — Claude with native tool calls and extended thinking.
//! - Google GenAI API — Gemini with native tool calls and thought signatures.
//!
//! Key types:
//! - [`types::AssistantTurn`] — normalized response: text, tool calls, token usage
//! - [`types::TokenUsage`]  — normalized usage counts
//! - [`types::ModelCapabilities`] — per-model capability flags
//! - [`types::StreamChunk`] — one streaming delta or final turn
//! - [`types::ToolCall`]    — one structured tool invocation
//! - [`Provider`]           — the core trait: `complete` + `capabilities`
//! - [`ProviderExt`]        — optional `stream` with a default no-op

mod anthropic;
mod bedrock;
mod error;
mod friendly_error;
#[allow(dead_code)]
#[allow(clippy::double_ended_iterator_last, clippy::while_let_on_iterator)]
mod gemini;
mod matrix;
#[allow(dead_code)]
#[allow(clippy::double_ended_iterator_last, clippy::while_let_on_iterator)]
mod openai;
mod registry;
mod router;
mod tool_args;
mod types;
mod vertex;

pub use error::Error;
pub use friendly_error::friendly_model_error;
pub use matrix::{entry_for, model_context_windows, model_labels, models_for_provider, MATRIX};
pub use registry::{
    all_descriptors, get_descriptor, ProviderConfig, ProviderDescriptor, ProviderField,
};
pub use router::{Provider, Router};
pub use tool_args::{normalize_tool_input, parse_tool_arguments, salvage_tool_args_from_text};
pub use types::{AssistantTurn, ModelCapabilities, StreamEvent, TokenUsage, ToolCall};
