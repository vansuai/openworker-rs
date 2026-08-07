//! OpenWorker Data Layer — Rust reimplementation of Memory/Sessions/Conversations/Automation.

mod automation;
mod conversation;
mod error;
mod inbox;
mod inbox_routing;
mod memory;
mod types;

pub use automation::{Schedule, ScheduledTask, TaskRun, TaskStore};
pub use conversation::{default_base_dir, ConversationStore};
pub use error::Error;
pub use inbox::{
    args_preview, InboxItem, InboxStore, KIND_APPROVAL, KIND_DIRECTORY, KIND_NOTIFICATION,
    KIND_PLAN, KIND_QUESTION, STATE_PENDING, STATE_RESOLVED, VIS_INBOX, VIS_INLINE,
};
pub use inbox_routing::{InboxBinding, InboxRouting, DEFAULT_INBOX};
pub use memory::{format_memories, MemoryStore, SQLiteMemoryStore};
pub use types::{MemoryItem, Scope, SessionRecord, SessionSummary};

// PyO3 bindings — compiled when `pyo3` feature is enabled.
// The `bindings` module is declared at crate root level.
// To use from Python: `import ocw_data` (the module name matches the Cargo package name).
#[cfg(feature = "pyo3")]
mod bindings;

#[cfg(feature = "pyo3")]
pub use bindings::{PyConversationStore, PySQLiteMemoryStore, PyTaskStore};
