import { useEffect, useRef, useState } from "react";
import {
  announceAutomationsChanged,
  cloudLogin,
  connectManaged,
  createAutomation,
  deleteAutomation,
  getAutomation,
  getAutomations,
  getCloudStatus,
  getConnectors,
  getRecentChannels,
  markAutomationSeen,
  updateAutomation,
  waitForCloudSignIn,
  type Automation,
  type AutomationRun,
  type CloudStatus,
  type Connector,
  type RecentChannel,
} from "../api";
import { ConnectorBadge } from "../connectors/ConnectorIcon";
import { AutomationQuickstart, Spinner } from "./AutomationQuickstart";
import { Icon } from "./Icon";
import { PanelHead } from "./IntegrationsView";
import { ChannelPicker } from "./SubscriptionsChip";

// Shared utility strings (the §28 page shell — mirrors IntegrationsView's constants).
const CARD = "rounded-xl2 border border-line bg-panel";

// Parse a simple "min hour * * dow" cron back into the time + frequency the editor uses.
// Falls back to 09:00 / daily for anything it doesn't recognize (e.g. agent-written crons).
function fromCron(cron?: string | null): { time: string; freq: string } {
  const parts = (cron || "").trim().split(/\s+/);
  if (parts.length !== 5) return { time: "09:00", freq: "daily" };
  const [m, h, , , dow] = parts;
  const hh = String(Math.min(23, Math.max(0, parseInt(h, 10) || 9))).padStart(2, "0");
  const mm = String(Math.min(59, Math.max(0, parseInt(m, 10) || 0))).padStart(2, "0");
  const freq = dow === "1-5" ? "weekdays" : dow === "0,6" || dow === "6,0" ? "weekends" : "daily";
  return { time: `${hh}:${mm}`, freq };
}

const fmt = (t: number | null) =>
  t ? new Date(t * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }) : "—";

// Map a simple time-of-day + frequency selection to a 5-field cron string.
function toCron(time: string, freq: string): string {
  const [h, m] = (time || "09:00").split(":").map((x) => parseInt(x, 10) || 0);
  const dow = freq === "weekdays" ? "1-5" : freq === "weekends" ? "0,6" : "*";
  return `${m} ${h} * * ${dow}`;
}

// The §28 page shell: full-bleed main, centered ≤4xl column — same as Connectors/Activity/Inbox.
function Shell({ children }: { children: React.ReactNode }) {
  return (
    <main className="main flex-1 min-w-0">
      <div className="flex-1 min-w-0 overflow-y-auto hairline-scroll">
        <div className="max-w-4xl mx-auto px-7 py-6">{children}</div>
      </div>
    </main>
  );
}

interface Props {
  // `task` gives the opened run session its context (banner + "Back to runs"; owner ask 2026-07-04).
  onOpenRun: (
    sessionId: string,
    workspace: string,
    agent: string,
    task?: { id: string; title: string },
  ) => void;
  onRunNow: (taskId: string, title?: string) => void;
  // Open directly on a task's detail (set by the run banner's "Back to runs").
  initialOpenId?: string | null;
}

export function ScheduledView({ onOpenRun, onRunNow, initialOpenId }: Props) {
  const [tasks, setTasks] = useState<Automation[]>([]);
  const [openId, setOpenId] = useState<string | null>(initialOpenId ?? null);
  const [showForm, setShowForm] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  // The sidebar's Scheduled band can retarget an ALREADY-open Automations surface —
  // initial state alone would ignore the change (UX-023).
  useEffect(() => {
    if (initialOpenId) setOpenId(initialOpenId);
  }, [initialOpenId]);

  const refresh = () => getAutomations().then(setTasks).catch(() => setTasks([]));
  useEffect(() => {
    refresh();
    const h = setInterval(refresh, 5000);
    return () => clearInterval(h);
  }, []);

  // Create from a payload, refresh the list, and open the new task's detail. `permissions`
  // rides through for quickstart recipes (§25 write grants).
  const create = async (payload: {
    title: string;
    instructions: string;
    cron?: string;
    permissions?: { tool: string; target?: string; access: "read" | "write" }[];
  }) => {
    setBusy(payload.title);
    try {
      const res = await createAutomation(payload);
      announceAutomationsChanged(); // new entry shows in the sidebar band right away
      await refresh();
      if (res.ok && res.task) {
        setShowForm(false);
        setOpenId(res.task.id);
      } else if (res.error) {
        alert(res.error);
      }
    } finally {
      setBusy(null);
    }
  };

  if (openId) {
    return (
      <TaskDetail
        id={openId}
        onBack={() => { setOpenId(null); refresh(); }}
        onOpenRun={onOpenRun}
        onRunNow={onRunNow}
      />
    );
  }

  const empty = tasks.length === 0;

  return (
    <Shell>
      <div className="flex items-start gap-3">
        <div className="flex-1 min-w-0">
          <PanelHead title="Automations" sub="Recurring tasks OpenWorker runs on a schedule." />
        </div>
        <button
          className="text-[12.5px] px-3 py-1.5 rounded-lg border border-lineStrong bg-panel hover:border-accent hover:text-accent shrink-0"
          onClick={() => setShowForm((v) => !v)}
        >
          + New automation
        </button>
      </div>

      <div className="text-[12px] text-faint flex gap-1.5 mb-4">
        <span aria-hidden>ⓘ</span>
        <span>
          Runs only while openworker-server is up — a missed schedule catches up once when it next
          starts.
        </span>
      </div>

      {showForm && (
        <NewAutomationForm
          busy={busy !== null}
          onCancel={() => setShowForm(false)}
          onCreate={create}
        />
      )}

      {/* The quickstart (§29): ONE template system — role recipes + generic templates, each
          card with §27 connector dots; picking one expands the configure card. */}
      {(empty || showForm) && <AutomationQuickstart busy={busy !== null} onCreate={create} />}

      {empty ? (
        !showForm && (
          <div className={CARD + " p-4 text-[12.5px] text-muted"}>
            No scheduled tasks yet — use a template above, click <strong>+ New automation</strong>,
            or just ask OpenWorker in a session.
          </div>
        )
      ) : (
        <div className="flex flex-col gap-2.5">
          {tasks.map((t) => (
            <div
              className={CARD + " sched-card px-4 py-3 cursor-pointer hover:border-lineStrong transition-colors"}
              key={t.id}
              onClick={() => setOpenId(t.id)}
            >
              <div className="flex items-center justify-between gap-2.5 mb-1">
                <span className="text-[13.5px] font-semibold truncate">{t.title}</span>
                <button
                  className="sched-card-del"
                  title="Delete automation"
                  aria-label={`Delete ${t.title}`}
                  onClick={async (e) => {
                    e.stopPropagation();
                    await deleteAutomation(t.id);
                    refresh();
                  }}
                >
                  <Icon name="trash" size={14} />
                </button>
              </div>
              <div className="flex items-center gap-1.5 text-[12px] text-muted">
                <Icon name="clock" size={13} className="text-faint shrink-0" />
                {t.enabled ? t.schedule : "Paused"} · next {fmt(t.next_run)} · {t.run_count} run{t.run_count === 1 ? "" : "s"}
                {t.last_status ? ` · last ${t.last_status}` : ""}
              </div>
            </div>
          ))}
        </div>
      )}
    </Shell>
  );
}

type AutomationPermission = { tool: string; target?: string; access: "read" | "write" };

/** Blank "+ New automation" form — not a template. Optional connections and the
 *  web_search standing grant ride out as `permissions` (the server's grant_entries). */
export function NewAutomationForm({
  busy,
  onCancel,
  onCreate,
}: {
  busy: boolean;
  onCancel: () => void;
  onCreate: (p: {
    title: string;
    instructions: string;
    cron?: string;
    permissions?: AutomationPermission[];
  }) => void;
}) {
  const [title, setTitle] = useState("");
  const [instructions, setInstructions] = useState("");
  const [time, setTime] = useState("09:00");
  const [freq, setFreq] = useState("daily");
  const [allowSearch, setAllowSearch] = useState(false);
  const [added, setAdded] = useState<string[]>([]);
  const [connectors, setConnectors] = useState<Connector[]>([]);
  const [cloud, setCloud] = useState<CloudStatus | null>(null);
  const [pendingConn, setPendingConn] = useState<string | null>(null);
  const [connFlow, setConnFlow] = useState<{ name: string; phase: "opening" | "waiting" } | null>(
    null,
  );
  const [signinPhase, setSigninPhase] = useState<"opening" | "waiting" | null>(null);
  const [recent, setRecent] = useState<RecentChannel[]>([]);
  const [channel, setChannel] = useState("");
  const [postConsent, setPostConsent] = useState(false);
  const signinPollRef = useRef<(() => void) | null>(null);

  const refresh = () => {
    getConnectors().then(setConnectors).catch(() => {});
    getCloudStatus().then(setCloud).catch(() => {});
  };
  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 3000);
    return () => clearInterval(t);
  }, []);
  useEffect(() => {
    if (connFlow && connectors.find((c) => c.name === connFlow.name)?.connected) setConnFlow(null);
  }, [connectors, connFlow]);
  useEffect(() => {
    if (added.some((n) => connectors.find((c) => c.name === n)?.channels)) {
      getRecentChannels().then(setRecent).catch(() => {});
    }
  }, [added, connectors]);
  useEffect(() => {
    return () => {
      signinPollRef.current?.();
      signinPollRef.current = null;
    };
  }, []);

  const connOf = (name: string) => connectors.find((c) => c.name === name);
  const remaining = connectors.filter((c) => c.available !== false && !added.includes(c.name));
  const missing = added.filter((n) => !connOf(n)?.connected);
  const channelReady = added.some((n) => {
    const c = connOf(n);
    return !!c?.channels && c.connected;
  });

  const startConnect = async (name: string) => {
    if (!cloud?.signed_in) {
      setPendingConn(name);
      return;
    }
    setConnFlow({ name, phase: "opening" });
    await connectManaged(name).catch(() => {});
    setConnFlow((f) => (f?.name === name ? { name, phase: "waiting" } : f));
    refresh();
  };

  const signInThenConnect = async () => {
    setSigninPhase("opening");
    await cloudLogin().catch(() => {});
    setSigninPhase("waiting");
    signinPollRef.current = waitForCloudSignIn(async (s) => {
      signinPollRef.current = null;
      setSigninPhase(null);
      if (!s?.signed_in) return;
      setCloud(s);
      if (pendingConn) {
        const name = pendingConn;
        setConnFlow({ name, phase: "opening" });
        await connectManaged(name).catch(() => {});
        setConnFlow((f) => (f?.name === name ? { name, phase: "waiting" } : f));
        setPendingConn(null);
        refresh();
      }
    });
  };

  const valid = title.trim() && instructions.trim();
  const gateHint =
    missing.length > 0
      ? `Connect ${missing.map((n) => connOf(n)?.title || n).join(" and ")} to continue`
      : "";

  const submit = () => {
    const permissions: AutomationPermission[] = [];
    if (allowSearch) permissions.push({ tool: "web_search", access: "write" });
    if (channelReady && postConsent && channel.trim()) {
      permissions.push({ tool: "send_message", target: channel.trim(), access: "write" });
    }
    onCreate({
      title: title.trim(),
      instructions: instructions.trim(),
      cron: toCron(time, freq),
      ...(permissions.length ? { permissions } : {}),
    });
  };

  return (
    <div className={CARD + " tmpl-form p-4 mb-4"} data-testid="na-form">
      <div className="text-[11px] uppercase tracking-[0.05em] text-faint mb-2.5">
        New automation
      </div>
      <input
        className="tmpl-input"
        placeholder="Title (e.g. Daily standup notes)"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
      />
      <textarea
        className="tmpl-input tmpl-textarea"
        placeholder="What should it do each run? (e.g. Summarize today's calendar and open tasks.)"
        value={instructions}
        onChange={(e) => setInstructions(e.target.value)}
      />
      <div className="tmpl-sched">
        <label className="tmpl-field">
          <span>At</span>
          <input
            type="time"
            className="tmpl-input tmpl-time"
            value={time}
            onChange={(e) => setTime(e.target.value)}
          />
        </label>
        <label className="tmpl-field">
          <span>Repeat</span>
          <select
            className="tmpl-input tmpl-select"
            value={freq}
            onChange={(e) => setFreq(e.target.value)}
          >
            <option value="daily">Every day</option>
            <option value="weekdays">Weekdays</option>
            <option value="weekends">Weekends</option>
          </select>
        </label>
      </div>

      <div className="mt-3">
        <div className="text-[11px] uppercase tracking-[0.05em] text-faint mb-1.5">Connections</div>
        {added.map((name) => {
          const c = connOf(name);
          const flow = connFlow?.name === name ? connFlow : null;
          return (
            <div key={name} className="border-b border-line" data-testid={`na-conn-${name}`}>
              <div className="flex items-center gap-3 py-2">
                {c && <ConnectorBadge connector={c} size={22} title={c.title} />}
                <span className="min-w-0 flex-1">
                  <span className="block text-[13px] font-medium">{c?.title || name}</span>
                  {c?.blurb && <span className="block text-[11.5px] text-faint">{c.blurb}</span>}
                </span>
                {c?.connected ? (
                  <span className="text-[12.5px] text-ok">✓ Connected</span>
                ) : flow ? (
                  <span className="inline-flex items-center gap-2 text-[12px] text-muted">
                    <Spinner />
                    {flow.phase === "opening"
                      ? "Opening browser…"
                      : `Waiting for ${c?.title || name}…`}
                  </span>
                ) : (
                  <button
                    type="button"
                    className="px-3 py-1 rounded-full border border-line text-[12.5px] hover:bg-paper"
                    onClick={() => startConnect(name)}
                    data-testid={`na-connect-${name}`}
                  >
                    Connect
                  </button>
                )}
                <button
                  type="button"
                  className="link text-[12px]"
                  onClick={() => {
                    setAdded((cur) => cur.filter((n) => n !== name));
                    if (connFlow?.name === name) setConnFlow(null);
                    if (pendingConn === name) setPendingConn(null);
                  }}
                  data-testid={`na-remove-${name}`}
                >
                  Remove
                </button>
              </div>
              {flow?.phase === "waiting" && (
                <div
                  className="flex items-start gap-2 bg-accentSoft/50 rounded-lg px-3 py-2 mb-2 text-[12px] text-muted"
                  data-testid="na-connect-wait"
                >
                  <span>↗</span>
                  <span className="flex-1 min-w-0">
                    <b className="text-ink font-medium">
                      Finish connecting {c?.title || name} in your browser.
                    </b>{" "}
                    Approve it there, then come back — this page updates by itself.
                  </span>
                  <button
                    type="button"
                    className="text-faint underline hover:text-muted shrink-0"
                    onClick={() => setConnFlow(null)}
                    data-testid="na-connect-cancel"
                  >
                    Cancel
                  </button>
                </div>
              )}
            </div>
          );
        })}
        {remaining.length > 0 && (
          <select
            className="tmpl-input mt-2"
            data-testid="na-add-conn"
            aria-label="Add connection"
            value=""
            onChange={(e) => {
              const name = e.target.value;
              if (!name) return;
              setAdded((cur) => (cur.includes(name) ? cur : [...cur, name]));
            }}
          >
            <option value="">Add connection…</option>
            {remaining.map((c) => (
              <option key={c.name} value={c.name}>
                {c.title}
              </option>
            ))}
          </select>
        )}
        {pendingConn && !cloud?.signed_in && (
          <div
            className="bg-accentSoft/50 rounded-xl px-4 py-3 mt-3 text-[12.5px] text-muted"
            data-testid="na-cloudpane"
          >
            <span className="block text-[13px] text-ink font-medium">
              One sign-in unlocks every one-click connection
            </span>
            Connections are brokered by OpenWorker Cloud — your tokens stay on this Mac.
            <div className="flex items-center gap-3 mt-2">
              {signinPhase ? (
                <>
                  <span className="inline-flex items-center gap-2 text-[12px]">
                    <Spinner />
                    {signinPhase === "opening" ? "Opening browser…" : "Waiting for sign-in…"}
                  </span>
                  {signinPhase === "waiting" && (
                    <button
                      type="button"
                      className="underline hover:text-muted text-[11.5px] text-faint"
                      onClick={() => {
                        signinPollRef.current?.();
                        signinPollRef.current = null;
                        setSigninPhase(null);
                      }}
                      data-testid="na-signin-cancel"
                    >
                      Cancel
                    </button>
                  )}
                </>
              ) : (
                <button
                  type="button"
                  className="px-3.5 py-1 rounded-full border border-line text-[12.5px] text-accent hover:bg-panel"
                  onClick={signInThenConnect}
                  data-testid="na-cloud-signin"
                >
                  Sign in to OpenWorker Cloud
                </button>
              )}
            </div>
          </div>
        )}
        {channelReady && (
          <div className="mt-3" data-testid="na-channel">
            <label className="block text-[12px] text-muted mb-1">Post to channel</label>
            <ChannelPicker
              value={channel}
              onChange={setChannel}
              recent={recent}
            />
            <label className="flex items-start gap-2.5 mt-2.5 text-[12.5px] text-muted select-none">
              <input
                type="checkbox"
                className="mt-0.5"
                checked={postConsent}
                onChange={(e) => setPostConsent(e.target.checked)}
                data-testid="na-post-consent"
              />
              <span>
                Allow this automation to post to the chosen channel without asking each time.
                Anything else still asks first.
              </span>
            </label>
          </div>
        )}
      </div>

      <label className="flex items-start gap-2.5 mt-3.5 text-[12.5px] text-muted select-none">
        <input
          type="checkbox"
          className="mt-0.5"
          checked={allowSearch}
          onChange={(e) => setAllowSearch(e.target.checked)}
          data-testid="na-search-consent"
        />
        <span>
          Allow this automation to search the web via your configured search provider without
          asking each time. Anything else still asks first.
        </span>
      </label>

      <div className="tmpl-form-actions">
        {gateHint && (
          <span className="text-[11.5px] text-faint mr-auto" data-testid="na-create-hint">
            {gateHint}
          </span>
        )}
        <button
          className="btn-primary sm"
          disabled={!valid || busy || missing.length > 0}
          onClick={submit}
          data-testid="na-create"
        >
          {busy ? "Creating…" : "Create automation"}
        </button>
        <button className="link" onClick={onCancel}>cancel</button>
      </div>
    </div>
  );
}

function TaskDetail({
  id,
  onBack,
  onOpenRun,
  onRunNow,
}: {
  id: string;
  onBack: () => void;
  onOpenRun: (
    sessionId: string,
    workspace: string,
    agent: string,
    task?: { id: string; title: string },
  ) => void;
  onRunNow: (taskId: string, title?: string) => void;
}) {
  const [task, setTask] = useState<Automation | null>(null);
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [editing, setEditing] = useState(false);
  const [title, setTitle] = useState("");
  const [instructions, setInstructions] = useState("");
  const [time, setTime] = useState("09:00");
  const [freq, setFreq] = useState("daily");
  const [saving, setSaving] = useState(false);

  // The seen mark AS OF opening — the "new" pills compare against this frozen value
  // while mark-seen advances the stored one (badge clears; highlights survive).
  const [seenMark, setSeenMark] = useState<number | null>(null);

  const refresh = () =>
    getAutomation(id)
      .then((d) => {
        if (!d.task) {
          // Deleted (or a stale reopen target): "Loading…" forever is a trap —
          // fall back to the overview (owner-hit 2026-07-20).
          onBack();
          return;
        }
        setTask(d.task);
        setRuns(d.runs || []);
        setSeenMark((cur) => (cur === null ? d.task?.seen_runs_at ?? 0 : cur));
      })
      .catch(() => {});
  useEffect(() => {
    setSeenMark(null);
    refresh();
    // Opening the detail IS reading it: advance the seen mark and nudge the
    // sidebar so the badge clears immediately (UX-023).
    markAutomationSeen(id)
      .then(() => announceAutomationsChanged())
      .catch(() => {});
  }, [id]);

  if (!task)
    return (
      <Shell>
        <div className="text-[13px] text-muted">Loading…</div>
      </Shell>
    );

  const startEdit = () => {
    setTitle(task.title);
    setInstructions(task.instructions);
    const { time: t, freq: f } = fromCron(task.schedule_raw?.cron);
    setTime(t);
    setFreq(f);
    setEditing(true);
  };
  const saveEdit = async () => {
    setSaving(true);
    try {
      await updateAutomation(id, {
        title: title.trim(),
        instructions: instructions.trim(),
        cron: toCron(time, freq),
      });
      await refresh();
      setEditing(false);
    } finally {
      setSaving(false);
    }
  };
  const toggle = async () => {
    await updateAutomation(id, { enabled: !task.enabled });
    refresh();
  };
  const remove = async () => {
    await deleteAutomation(id);
    announceAutomationsChanged(); // the sidebar band must not wait out its poll
    onBack();
  };

  return (
    <Shell>
      <button className="text-[13px] text-muted hover:text-ink mb-3" onClick={onBack}>
        ← Automations
      </button>
      <div className="sched-detail">
        <div className="sched-detail-head">
          {editing ? (
            <input
              className="tmpl-input sched-edit-title"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder="Title"
            />
          ) : (
            <h2 className="text-[18px] font-semibold tracking-tight">{task.title}</h2>
          )}
          <div className="sched-actions">
            {editing ? (
              <>
                <button className="btn-primary sm" disabled={saving || !title.trim() || !instructions.trim()} onClick={saveEdit}>
                  {saving ? "Saving…" : "Save"}
                </button>
                <button className="link" onClick={() => setEditing(false)}>cancel</button>
              </>
            ) : (
              <>
                <button className="btn-primary sm" onClick={() => onRunNow(id, task.title)}>
                  ▶ Run now
                </button>
                <button className="btn sm" onClick={startEdit}>Edit</button>
                <button className="btn sm danger-btn" onClick={remove}>
                  <Icon name="trash" size={14} /> Delete
                </button>
              </>
            )}
          </div>
        </div>

        {editing ? (
          <div className="tmpl-sched sched-edit-sched">
            <label className="tmpl-field">
              <span>At</span>
              <input type="time" className="tmpl-input tmpl-time" value={time} onChange={(e) => setTime(e.target.value)} />
            </label>
            <label className="tmpl-field">
              <span>Repeat</span>
              <select className="tmpl-input tmpl-select" value={freq} onChange={(e) => setFreq(e.target.value)}>
                <option value="daily">Every day</option>
                <option value="weekdays">Weekdays</option>
                <option value="weekends">Weekends</option>
              </select>
            </label>
          </div>
        ) : (
          <div className="conn-meta">
            <label className="switch">
              <input type="checkbox" checked={task.enabled} onChange={toggle} />
              <span className="slider" />
            </label>{" "}
            {task.enabled ? `Active · next ${fmt(task.next_run)}` : "Paused"} · {task.schedule}
          </div>
        )}

        <div className="sa-sub">Instructions</div>
        {editing ? (
          <textarea
            className="tmpl-input tmpl-textarea sched-edit-instr"
            value={instructions}
            onChange={(e) => setInstructions(e.target.value)}
          />
        ) : (
          <div className="sched-instructions">{task.instructions}</div>
        )}

        {(task.always_allowed || []).length > 0 && (
          <>
            <div className="sa-sub">Allowed without asking</div>
            <div className="dim" style={{ marginBottom: 8, fontSize: 12.5 }}>
              Standing approvals this automation may use — everything else still asks first.
            </div>
            <div className="sched-grants" data-testid="task-grants">
              {(task.always_allowed || []).map((rule) => (
                <div className="sched-grant" key={rule.entry}>
                  <span className="sched-grant-rule">
                    <code>{rule.tool}</code>
                    {rule.target && <span className="sched-grant-target"> → {rule.target}</span>}
                  </span>
                  <button
                    className="link"
                    title="This automation will ask for approval again"
                    onClick={async () => {
                      await updateAutomation(id, { revoke: rule.entry });
                      refresh();
                    }}
                  >
                    Revoke
                  </button>
                </div>
              ))}
            </div>
          </>
        )}

        <div className="sa-sub">Runs</div>
        <div className="dim" style={{ marginBottom: 8, fontSize: 12.5 }}>
          Each run is a live conversation — open one to see what the agent did and ask a follow-up.
        </div>
        {runs.length === 0 && <div className="dim">No runs yet.</div>}
        {runs.map((r) => (
          <div
            className="sched-run open"
            key={r.run_id}
            onClick={() =>
              r.session_id &&
              onOpenRun(r.session_id, task.workspace, task.agent, {
                id: task.id,
                title: task.title,
              })
            }
            title="Open this run's conversation"
          >
            <div className="sched-run-row">
              <span>
                {seenMark !== null && r.started_at > seenMark && (
                  <span className="run-new-pill" data-testid="run-new">new</span>
                )}
                {fmt(r.started_at)} · <span className={"run-" + r.status}>{r.status}</span> · {r.trigger}
                {r.model && <span className="dim"> · {r.model.includes(":") ? r.model.split(":").slice(1).join(":") : r.model}</span>}
                {r.artifacts.length > 0 && <span className="dim"> · {r.artifacts.length} file(s)</span>}
              </span>
              <span className="sched-run-go" aria-hidden>
                Open ›
              </span>
            </div>
            {r.result_text && <div className="sched-run-peek">{r.result_text}</div>}
            {r.error && <div className="mcp-error">{r.error}</div>}
          </div>
        ))}
      </div>
    </Shell>
  );
}
