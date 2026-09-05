//! OpenWorker Server — Rust reimplementation of `coworker/server/`.
//!
//! Axum HTTP/WebSocket server providing:
//! - REST API (`/v1/*`) — sessions, personas, connectors, inbox, memory, etc.
//! - WebSocket session API (`/ws/session/{session_id}`) — live agent interaction
//! - OpenAI-compatible completions (`/v1/chat/completions`)

pub mod agents;
pub mod app;
pub mod attachments;
pub mod automations;
pub mod board_tools;
pub mod cloud;
pub mod config;
pub mod connector_accounts;
pub mod connectors;
pub mod error;
pub mod events_ws;
pub mod environment;
pub mod mcp;
pub mod mcp_runtime;
pub mod memory_tools;
pub mod persona_manifest;
pub mod personas;
pub mod project;
pub mod projects;
pub mod scheduler;
pub mod settings;
pub mod state;
pub mod stores;
pub mod subsystems;
pub mod team_tick;
pub mod teams;
pub mod ws;
