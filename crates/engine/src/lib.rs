//! OpenWorker engine — the owned agent loop.
//!
//! Async, with blocking provider/tool calls wrapped in `tokio::task::spawn_blocking` so
//! the loop (and any UI consuming its events) stays responsive. One user turn spans many
//! model↔tool iterations until the model stops requesting tools, a rail trips, or it's
//! interrupted. Low-risk tool calls execute concurrently; writes/shell stay strictly ordered.
//!
//! Approvals are handled out-of-band via an injected async `approver`: when the permission
//! engine says `needs_user`, the engine emits `PermissionRequired` and awaits the approver.

pub mod compaction;
pub mod engine;
pub mod events;
pub mod permissions;
pub mod provenance;
pub mod reviewer;
pub mod subagent;
pub mod tool_registry;
pub mod tool_types;
pub mod types;

pub use compaction::{
    apply_to_outbound, build_state_with_summary, compacted_block, estimate_tokens,
    extract_user_messages_default, extract_working_state, is_context_overflow, keep_tokens_for_trigger,
    pick_boundary, should_compact, should_compact_default, summarizer_messages, trigger_tokens,
    trigger_tokens_default, trim_state_default, CompactionState, CONTINUATION_CONTRACT,
    DEFAULT_CAP_TOKENS, DEFAULT_CONTEXT_WINDOW, DEFAULT_THRESHOLD_PCT, KEEP_RECENT_FRACTION,
    SUMMARY_MAX_TOKENS, SUMMARY_SYSTEM_PROMPT,
};
pub use engine::{
    ApprovalOutcome, Approver, AuditSink, DirectoryRequester, DirectoryResult,
    PermissionRequest, PlanApprover, PlanResult, QuestionAsker, ReviewerFn, TurnEngine,
};
pub use events::{Event, EventData, EventType};
pub use permissions::{
    classify_risk, classify_risk_with, standing_target_candidate, standing_target_candidate_with,
    target_arg_for, Decision, Mode, PermissionEngine, RiskClass,
};
pub use provenance::{
    attach_approval_display, command_paths, created_paths, referenced_paths, resolve,
    ApprovalOrigin, Match as ProvenanceMatch, Origin as FileOrigin, SessionFiles, DOWNLOADED,
    WRITTEN,
};
pub use reviewer::{
    build_review_prompt, parse_reviewer_response, ReviewerDecision, Verdict as ReviewerVerdict,
    AGENT_DENY_MESSAGE, INSTRUCTIONS as REVIEWER_INSTRUCTIONS, REVIEWER_PAUSED_TEXT,
    REVIEWER_TRIP,
};
pub use subagent::{run_explorer, ExplorerReport, EXPLORER_INSTRUCTIONS, EXPLORER_MAX_ITERATIONS};
pub use tool_registry::ToolRegistry;
pub use tool_types::{Error as ToolError, ToolArg, ToolFn, ToolResult, ToolSchema, ToolSpec};
pub use types::{Message, ToolCall};
