//! OpenWorker messaging connectors — adapters, gateway, senders, integration.
//!
//! This crate is the Rust reimplementation of `coworker/connectors/`:
//!
//! - [`base`]: platform-agnostic value types and the
//!   [`BasePlatformAdapter`](base::BasePlatformAdapter) trait.
//! - [`adapters`]: pure mappers + concrete Slack/Telegram/Email adapter shells.
//! - [`gateway`]: orchestrator that starts/stops adapters and dispatches inbound
//!   messages to handlers (**lifecycle shell present; Socket Mode / poll not yet**).
//! - [`senders`]: outbound helpers used by the agent's `send_message` /
//!   `send_file` tools.
//! - [`integration`]: third-party API helpers (GitHub Issues/PR, Gmail, Google
//!   Calendar, HubSpot).

pub mod adapters;
pub mod base;
pub mod gateway;
pub mod integration;
pub mod senders;
pub mod slack_directory;

pub use adapters::{
    slack_event_to_event, slack_to_message_source, telegram_message_to_event, EmailAdapter,
    SlackAdapter, TelegramAdapter,
};
pub use base::{
    format_target, parse_target, BasePlatformAdapter, ButtonSpec, InteractionEvent,
    InteractionHandler, MessageEvent, MessageHandler, MessageSource, MessageType, SendResult,
    SessionSource,
};
pub use gateway::{noop_handler, AdapterStatus, Gateway, Platform};
pub use integration::{
    register_all as register_integration_tools, IntegrationContext, SecretResolver,
    WorkspaceRoot,
};
pub use slack_directory::{clear_cache as clear_slack_directory_cache, list_channels, list_members};
pub use senders::{
    default_file_senders, default_senders, send_slack, send_slack_file, send_slack_interactive,
    send_telegram, slack_blocks, split_slack_chat_id, FileSender, Sender,
};
pub type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;