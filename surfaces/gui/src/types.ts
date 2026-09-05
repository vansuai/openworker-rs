export type EventType =
  | "ready"
  | "inbound"
  | "turn_start"
  | "assistant_delta"
  | "reasoning_delta"
  | "assistant_message"
  | "tool_proposed"
  | "permission_required"
  | "directory_requested"
  | "question_requested"
  | "plan_proposed"
  | "tool_started"
  | "tool_finished"
  | "iteration_end"
  | "turn_end"
  | "error"
  | "input_rejected"
  | "interrupted"
  | "model_changed"
  | "turn_done";

export interface WsEvent {
  type: EventType;
  data: any;
}

// Re-exported for transcript items below. Lives in api.ts (the REST/WS contract source of truth);
// type-only import, so there's no runtime cycle with api.ts's `import type { ... } from "./types"`.
import type { MessageSource } from "./api";

// "always_task" persists to the owning automation's task record (standing scoped
// approval, UX-DECISIONS §25) — offered only on automation-run approval cards, in-app.
// "stale" marks an approval that the engine abandoned without an answer (timeout,
// disconnected channel, turn_end). UI collapses the card; nothing was actually
// decided by the user.
export type ApprovalDecision =
  | "once"
  | "deny"
  | "always_tool"
  | "always_command"
  | "always_domain"
  | "always_task"
  // OPE-136 §4: durable per-MCP-tool trust — writes a rule to the user-local
  // override store; survives sessions; revocable on the server's detail page.
  | "always_trust"
  // OPE-136 run grant: covers the exact tool for the remainder of the current
  // answer only; in-memory, cleared at the run boundary. EXTERNAL family only.
  | "this_run"
  | "readonly_session"
  | "stale";

export interface TodoItem {
  content: string;
  status: "pending" | "in_progress" | "done";
}

// Per-round-trip token counts, as attached by the server to assistant messages and
// the assistant_message event (`{model, input, output, cache_read, cache_write}`).
// Absent on older servers and on backends that don't report usage.
export interface TurnUsage {
  model?: string | null;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

// Per-session accumulation, keyed by model id (multiple models when the user
// switched mid-session). `context` = the latest round-trip's prompt-side total —
// what currently occupies the active model's context window.
export interface SessionUsage {
  byModel: Record<string, TurnUsage>;
  context: number;
}

export interface SessionInfo {
  session_id: string;
  title?: string;
  workspace: string;
  agent: string;
  model: string;
  mode: string;
  updated_at: string | null;
  messages: number;
  pinned?: boolean;
  archived?: boolean;
  // Inbox items awaiting this session (the amber attention count that bubbles up the sidebar).
  attention?: number;
  // working = in-flight turn; sleeping = a self-wake is pending; idle = neither. A count-less dot.
  liveness?: "working" | "sleeping" | "idle";
  // Channels this session listens to (inbound subscriptions).
  subscriptions?: string[];
  // §31: set when the session was spawned by a platform mention rather than the user —
  // machine key ("slack") + display label ("#general · T0ABCD"). Drives the sidebar's
  // "From Slack" group and the row's platform icon.
  origin?: string;
  origin_label?: string;
}

// Attachments (images, PDFs, text files) sent with a user message.
export interface Attachment {
  kind: "image" | "text" | "pdf";
  name: string;
  mime?: string;
  data_url?: string; // images + PDFs
  text?: string; // text files
}

// Transcript items
// `ts` = unix seconds (the server's canonical-message stamp; live items stamp locally).
// Optional: sessions saved before the server stamped timestamps have none.
export type Item =
  | { kind: "user"; text: string; attachments?: Attachment[]; ts?: number }
  // A connector-delivered inbound message (Slack/Salesforce/…), rendered as a structured card
  // (ConnectorMessageCard) instead of a plain user bubble. Generalizes to any connector via the
  // registry — no per-connector special-casing.
  | { kind: "connector"; source: MessageSource }
  | { kind: "assistant"; text: string; ts?: number; reasoning?: string }
  // `hidden` = results the user's privacy filters removed before the agent saw them
  // (from the tool message's `_display` sidecar; the agent-visible content has no trace).
  // `standingRule` = the task-scoped rule that auto-allowed this call ("tool → target").
  | { kind: "tool"; id: string; name: string; args: any; status: string; preview?: string; hidden?: number; standingRule?: string }
  | {
      kind: "approval";
      name: string;
      args: any;
      reason: string;
      category?: string;
      // The exact target a standing rule could pin (server-computed) — with a run
      // context, the card offers "Allow every time" (§25).
      standingTarget?: string;
      // web_search only (§1.9): the LIVE configured provider name.
      searchProvider?: string;
      // OPE-114 §1: file the agent itself created/downloaded this session.
      provenance?: string;
      // Auto-Approve reviewer answered `unsure` and raised this card.
      reviewerUnsure?: string;
      // Server-classified: this shell command only reads locally.
      readonlyOk?: boolean;
      // OPE-136: where an MCP call goes ({transport, host}).
      mcpDestination?: { transport: string; host?: string };
      resolved?: ApprovalDecision;
      // Server-supplied Inbox id (attended WS broadcast + unattended mirror).
      itemId?: string;
      // Server-supplied OpenAI tool_call_id.
      toolCallId?: string;
    }
  | {
      kind: "dirreq";
      reason: string;
      path?: string;
      writable?: boolean;
      resolved?: "granted" | "denied";
    }
  | {
      kind: "planreq";
      plan: string;
      resolved?: "approved" | "rejected";
    }
  | {
      // Decomposition gate (agent teams): lead proposes work items; approval creates them on the board.
      kind: "itemsreq";
      items: { title: string; criteria: string }[];
      note?: string;
      resolved?: "approved" | "rejected";
    }
  | {
      // A live ask_user prompt (attended sessions answer inline; unattended ones route to the Inbox).
      kind: "question";
      question: string;
      header?: string;
      options?: QuestionOption[];
      allow_text?: boolean;
      multi?: boolean;
      resolved?: string;
    }
  | { kind: "notice"; tone: "info" | "warn"; text: string; retriable?: boolean };

/** Rich or plain ask_user options (OPE-51). */
export type QuestionOption =
  | string
  | {
      label: string;
      description?: string;
      recommended?: boolean;
      preview?: string;
    };
