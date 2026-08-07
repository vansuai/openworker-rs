//! OpenWorker engine — the owned agent loop.
//!
//! Async, with blocking provider/tool calls wrapped in `tokio::task::spawn_blocking` so
//! the loop (and any UI consuming its events) stays responsive. One user turn spans many
//! model↔tool iterations until the model stops requesting tools, a rail trips, or it's
//! interrupted. Low-risk tool calls execute concurrently; writes/shell stay strictly ordered.
//!
//! Approvals are handled out-of-band via an injected async `approver`: when the permission
//! engine says `needs_user`, the engine emits `PermissionRequired` and awaits the approver.

pub mod engine;
pub mod events;
pub mod permissions;
pub mod subagent;
pub mod tool_registry;
pub mod tool_types;
pub mod types;

pub use engine::{
    ApprovalOutcome, Approver, DirectoryResult, EngineCallbacks, PermissionRequest, PlanResult,
    TurnEngine,
};
pub use events::{Event, EventData, EventType};
pub use permissions::{Decision, Mode, PermissionEngine};
pub use subagent::{run_explorer, ExplorerReport, EXPLORER_INSTRUCTIONS, EXPLORER_MAX_ITERATIONS};
pub use tool_registry::ToolRegistry;
pub use tool_types::{Error as ToolError, ToolArg, ToolFn, ToolResult, ToolSchema, ToolSpec};
pub use types::{Message, ToolCall};
