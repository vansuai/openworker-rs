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
        Self {
            event_type: EventType::PermissionRequired,
            data: EventData::PermissionRequired {
                name,
                arguments,
                reason,
                category,
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
}
