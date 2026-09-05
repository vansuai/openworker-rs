//! Event model — the contract between the turn engine and any surface (TUI/GUI/IDE).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// All event types emitted by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    TurnStart,
    AssistantDelta,
    ReasoningDelta,
    AssistantMessage,
    ToolProposed,
    PermissionRequired,
    DirectoryRequested,
    QuestionRequested,
    PlanProposed,
    ToolStarted,
    ToolFinished,
    IterationEnd,
    TurnEnd,
    Error,
    Interrupted,
    /// Compaction started — surfaces show a transient progress signal.
    Compacting,
    /// Outbound history was compacted (summary or trim).
    Compacted,
}

/// One event emitted by the engine.
///
/// Surfaces (TUI, GUI, IDE) subscribe to this stream to render the live turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub data: EventData,
}

impl Event {
    pub fn new(event_type: EventType, data: EventData) -> Self {
        Self { event_type, data }
    }

    pub fn turn_start(input: Value) -> Self {
        Self {
            event_type: EventType::TurnStart,
            data: EventData::TurnStart { input },
        }
    }

    pub fn assistant_delta(text: String) -> Self {
        Self {
            event_type: EventType::AssistantDelta,
            data: EventData::AssistantDelta { text },
        }
    }

    pub fn reasoning_delta(text: String) -> Self {
        Self {
            event_type: EventType::ReasoningDelta,
            data: EventData::ReasoningDelta { text },
        }
    }

    pub fn assistant_message(
        text: Option<String>,
        tool_call_names: Vec<String>,
        reasoning: Option<String>,
        usage: Option<Value>,
    ) -> Self {
        Self {
            event_type: EventType::AssistantMessage,
            data: EventData::AssistantMessage {
                text,
                tool_call_names,
                reasoning,
                usage,
            },
        }
    }

    pub fn tool_proposed(name: String, arguments: Value) -> Self {
        Self {
            event_type: EventType::ToolProposed,
            data: EventData::ToolProposed { name, arguments },
        }
    }

    pub fn permission_required(
        name: String,
        arguments: Value,
        reason: String,
        category: String,
    ) -> Self {
        let standing_target = arguments
            .as_object()
            .and_then(|m| crate::permissions::standing_target_candidate(&name, m));
        Self {
            event_type: EventType::PermissionRequired,
            data: EventData::PermissionRequired {
                name,
                arguments,
                reason,
                category,
                tool_call_id: None,
                standing_target,
            },
        }
    }

    /// Variant carrying the originating `tool_call_id` so a downstream surface
    /// (GUI, Inbox, Slack reply) can round-trip an approval response back to
    /// the same in-flight permission request, even when several are queued.
    pub fn permission_required_for(
        name: String,
        arguments: Value,
        reason: String,
        category: String,
        tool_call_id: Option<String>,
    ) -> Self {
        let standing_target = arguments
            .as_object()
            .and_then(|m| crate::permissions::standing_target_candidate(&name, m));
        Self {
            event_type: EventType::PermissionRequired,
            data: EventData::PermissionRequired {
                name,
                arguments,
                reason,
                category,
                tool_call_id,
                standing_target,
            },
        }
    }

    pub fn directory_requested(reason: String, path: String, writable: bool) -> Self {
        Self {
            event_type: EventType::DirectoryRequested,
            data: EventData::DirectoryRequested {
                reason,
                path,
                writable,
            },
        }
    }

    pub fn question_requested(question: String, tool_call_id: String) -> Self {
        Self {
            event_type: EventType::QuestionRequested,
            data: EventData::QuestionRequested {
                question,
                tool_call_id,
            },
        }
    }

    pub fn plan_proposed(plan: String) -> Self {
        Self {
            event_type: EventType::PlanProposed,
            data: EventData::PlanProposed { plan },
        }
    }

    pub fn tool_started(name: String) -> Self {
        Self {
            event_type: EventType::ToolStarted,
            data: EventData::ToolStarted { name },
        }
    }

    pub fn tool_finished(
        name: String,
        status: String,
        result_preview: Option<String>,
        reason: Option<String>,
        display: Option<Value>,
        standing_rule: Option<String>,
    ) -> Self {
        Self {
            event_type: EventType::ToolFinished,
            data: EventData::ToolFinished {
                name,
                status,
                result_preview,
                reason,
                display,
                standing_rule,
                tool_call_id: None,
            },
        }
    }

    /// Variant carrying the originating `tool_call_id` — used by the engine to
    /// pair a `tool_finished: denied | error` with the matching
    /// `permission_required` so the GUI can clear the now-stale approval card.
    pub fn tool_finished_for(
        name: String,
        status: String,
        result_preview: Option<String>,
        reason: Option<String>,
        display: Option<Value>,
        standing_rule: Option<String>,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            event_type: EventType::ToolFinished,
            data: EventData::ToolFinished {
                name,
                status,
                result_preview,
                reason,
                display,
                standing_rule,
                tool_call_id,
            },
        }
    }

    pub fn iteration_end(iteration: usize) -> Self {
        Self {
            event_type: EventType::IterationEnd,
            data: EventData::IterationEnd { iteration },
        }
    }

    pub fn turn_end(status: String, iterations: usize) -> Self {
        Self {
            event_type: EventType::TurnEnd,
            data: EventData::TurnEnd { status, iterations },
        }
    }

    pub fn error(error: String, error_type: String) -> Self {
        Self {
            event_type: EventType::Error,
            data: EventData::Error { error, error_type },
        }
    }

    pub fn interrupted(iterations: usize) -> Self {
        Self {
            event_type: EventType::Interrupted,
            data: EventData::Interrupted { iterations },
        }
    }

    pub fn compacting() -> Self {
        Self {
            event_type: EventType::Compacting,
            data: EventData::Compacting {},
        }
    }

    pub fn compacted(text: String) -> Self {
        Self {
            event_type: EventType::Compacted,
            data: EventData::Compacted { text },
        }
    }
}

/// Payload variants for each event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventData {
    TurnStart {
        input: Value,
    },

    AssistantDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        text: Option<String>,
        tool_call_names: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
    },

    ToolProposed {
        name: String,
        arguments: Value,
    },
    PermissionRequired {
        name: String,
        arguments: Value,
        reason: String,
        category: String,
        /// The provider-side `id` of the originating tool call (e.g. `call_1`).
        /// Optional so older event producers can omit it; the GUI treats it as
        /// opaque and uses it to clear stale approval cards on `tool_finished`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tool_call_id: Option<String>,
        /// The target value iff this call is eligible for a task-scoped standing
        /// rule (§25 "Allow every time"). None → the call is ineligible and keeps
        /// parking approvals as today. Mirrors Python's `standing_rule_candidate`.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        standing_target: Option<String>,
    },
    DirectoryRequested {
        reason: String,
        path: String,
        writable: bool,
    },
    QuestionRequested {
        question: String,
        tool_call_id: String,
    },
    PlanProposed {
        plan: String,
    },
    ToolStarted {
        name: String,
    },
    ToolFinished {
        name: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_preview: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        standing_rule: Option<String>,
        /// Same meaning as `PermissionRequired::tool_call_id`; the GUI uses
        /// the pair to match a finished tool back to the approval card it
        /// produced (and clear that card if the tool was denied / errored).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tool_call_id: Option<String>,
    },
    IterationEnd {
        iteration: usize,
    },
    TurnEnd {
        status: String,
        iterations: usize,
    },
    Error {
        error: String,
        error_type: String,
    },
    Interrupted {
        iterations: usize,
    },
    Compacting {},
    Compacted {
        text: String,
    },
}
