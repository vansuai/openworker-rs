//! OpenWorker Data Layer — Rust reimplementation of Memory/Sessions/Conversations/Automation.

mod automation;
mod chat;
mod conversation;
mod error;
mod inbox;
mod inbox_routing;
mod journal;
mod memory;
mod overrides;
mod team_registry;
mod teams;
mod types;

pub use automation::{Schedule, ScheduledTask, TaskRun, TaskStore};
pub use chat::{ChatMember, ChatStore};
pub use conversation::{default_base_dir, ConversationStore};
pub use error::Error;
pub use inbox::{
    args_preview, InboxItem, InboxStore, KIND_APPROVAL, KIND_DIRECTORY, KIND_NOTIFICATION,
    KIND_PLAN, KIND_QUESTION, STATE_PENDING, STATE_RESOLVED, VIS_INBOX, VIS_INLINE,
};
pub use inbox_routing::{
    reply_intent, resolve_from_reply, InboxBinding, InboxRouting, DEFAULT_INBOX,
};
pub use journal::{JournalStore, GENESIS, JOURNAL_BODY_LIMIT, JOURNAL_KINDS};
pub use memory::{format_memories, MemoryStore, SQLiteMemoryStore};
pub use overrides::RiskOverrideStore;
pub use team_registry::{Team, TeamRegistry, TeamWorker};
pub use teams::{
    space_for_workspace, stored_name, validate_stored_name, Actor, AttachmentStore, BoardError,
    BoardItem, Role, TeamStore,
};
pub use types::{MemoryItem, Scope, SessionRecord, SessionSummary};

/// Truncate `s` to at most `max_chars` characters (not bytes). Never splits a
/// multi-byte UTF-8 sequence — tool output regularly carries CJK text, and a
/// byte-offset slice into it panics (owner-hit 2026-08-08: `preview()` crash on
/// a weather report killed the turn task).
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Truncate `s` to fit within `max_bytes`, rounding the cut down to the nearest
/// char boundary so the result is always valid UTF-8.
pub fn clip_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// PyO3 bindings — compiled when `pyo3` feature is enabled.
// The `bindings` module is declared at crate root level.
// To use from Python: `import ocw_data` (the module name matches the Cargo package name).
#[cfg(feature = "pyo3")]
mod bindings;

#[cfg(feature = "pyo3")]
pub use bindings::{PyConversationStore, PySQLiteMemoryStore, PyTaskStore};

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn truncate_chars_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("天气真好", 2), "天气");
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("天气", 0), "");
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn clip_utf8_rounds_down_to_char_boundary() {
        // '天' is 3 bytes: a 4-byte budget can only hold one full char.
        assert_eq!(clip_utf8("天气", 4), "天");
        assert_eq!(clip_utf8("天气", 6), "天气");
        assert_eq!(clip_utf8("天气", 100), "天气");
        assert_eq!(clip_utf8("天气", 0), "");
        assert_eq!(clip_utf8("abcd", 2), "ab");
    }
}
