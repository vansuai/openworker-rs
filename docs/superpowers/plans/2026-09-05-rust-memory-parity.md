# Rust Memory Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Tauri/Rust sidecar memory match Python’s core closed loop: SQLite persistence, four agent tools, correct prompt injection (GLOBAL+WORKSPACE, `#id`, user_rules, saving gate).

**Architecture:** Add a `MemoryBackend` trait over existing Vec/`SQLiteMemoryStore`; wire production to `data_dir/coworker.db`; register memory tools in `build_builtin_registry` like scheduling tools; fix REST + `build_system_messages` + per-turn off notice.

**Tech Stack:** Rust (`ocw_data`, `ocw-server`, `ocw_engine` ToolRegistry), SQLite via rusqlite, Axum REST, existing GUI Settings (no GUI changes in this plan).

**Design doc:** [docs/superpowers/specs/2026-09-05-rust-memory-parity-design.md](../specs/2026-09-05-rust-memory-parity-design.md)

## Global Constraints

- Follow Python **runtime** behavior in `coworker/agent.py` + `tests/test_memory.py` / `test_memory_api.py` (not the outdated docstring in `settings.py`).
- Off = stop learning, not amnesia.
- Do not implement `memory_saved` toast/Undo in this plan.
- Workspace key for this plan: `crate::projects::project_key(workspace)` only.
- Prefer small focused modules; put agent tools in `crates/server/src/memory_tools.rs` (needs store + optional broadcast later).
- TDD: failing test before implementation for each task.
- Do not edit unrelated files; do not expand into bindings/rekey unless a test forces a minimal stub.

## File map

| File | Role |
|------|------|
| `crates/data/src/types.rs` | Add `summary` to `MemoryItem` |
| `crates/data/src/memory.rs` | Trait, summary column, `render_memory_block`, index mode |
| `crates/data/src/lib.rs` | Re-exports |
| `crates/server/src/state.rs` | SQLite wire-up, injection, guidance copy |
| `crates/server/src/app.rs` | REST workspace/summary/scope |
| `crates/server/src/memory_tools.rs` | **Create** — four tools |
| `crates/server/src/ws.rs` | Register tools; per-turn off notice |
| `crates/server/src/lib.rs` | `mod memory_tools` |
| `crates/server/src/projects.rs` | `format_user_rules` helper (or next to settings) |

---

### Task 1: Data model — `summary` + schema migrate

**Files:**
- Modify: `crates/data/src/types.rs`
- Modify: `crates/data/src/memory.rs`
- Test: `crates/data/src/memory.rs` (existing `#[cfg(test)]`)

**Interfaces:**
- Produces: `MemoryItem.summary: Option<String>`; SQLite `summary` column; `add`/`update` accept `summary`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn sqlite_persists_summary() {
    let dir = tempdir().unwrap();
    let store = SQLiteMemoryStore::open(dir.path().join("m.db")).unwrap();
    let item = store
        .add("full text", Scope::Global, None, Some("one line"), None, None)
        .unwrap();
    assert_eq!(item.summary.as_deref(), Some("one line"));
    let got = store.get(item.id).unwrap().unwrap();
    assert_eq!(got.summary.as_deref(), Some("one line"));
}
```

(Adjust `add` signature in the test to match the signature you introduce in Step 3 — keep positional args consistent with existing Rust: content, scope, key, summary, workspace, session_id.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cd crates && cargo test -p ocw-data sqlite_persists_summary -- --nocapture`  
Expected: FAIL (no summary field / wrong arity)

- [ ] **Step 3: Implement**

1. Add to `MemoryItem`:
```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub summary: Option<String>,
```
2. Update in-memory `MemoryStore::add` / `update` to take `summary: Option<&str>` and store it.
3. On `SQLiteMemoryStore::open`: after `CREATE TABLE`, run Python-style migrate:
```rust
let cols: HashSet<String> = /* PRAGMA table_info */;
if !cols.contains("summary") {
    conn.execute("ALTER TABLE memories ADD COLUMN summary TEXT", [])?;
}
```
Include `summary` in new `CREATE TABLE` for fresh DBs.
4. Thread `summary` through `INSERT`/`SELECT`/`UPDATE`.

- [ ] **Step 4: Run tests**

Run: `cd crates && cargo test -p ocw-data memory -- --nocapture`  
Expected: PASS (fix any constructors that build `MemoryItem` literally)

- [ ] **Step 5: Commit**

```bash
git add crates/data/src/types.rs crates/data/src/memory.rs
git commit -m "$(cat <<'EOF'
feat(data): add memory summary field and SQLite column migrate

EOF
)"
```

---

### Task 2: `MemoryBackend` trait + `render_memory_block`

**Files:**
- Modify: `crates/data/src/memory.rs`
- Modify: `crates/data/src/lib.rs`

**Interfaces:**
- Produces:
```rust
pub trait MemoryBackend: Send + Sync {
    fn add(&self, content: &str, scope: Scope, key: Option<&str>, summary: Option<&str>,
           workspace: Option<&str>, session_id: Option<&str>) -> Result<MemoryItem, Error>;
    fn get(&self, item_id: i64) -> Result<Option<MemoryItem>, Error>;
    fn list(&self, scope: Option<Scope>, workspace: Option<&str>, session_id: Option<&str>)
        -> Result<Vec<MemoryItem>, Error>;
    fn update(&self, item_id: i64, content: &str, summary: Option<&str>)
        -> Result<Option<MemoryItem>, Error>;
    fn delete(&self, item_id: i64) -> Result<bool, Error>;
    fn delete_all(&self) -> Result<usize, Error>;
}
```
- Produces: `pub fn render_memory_block(items: &[MemoryItem]) -> String` (threshold 8000, newest 10 full) matching Python `render_memory_block` / `format_memories` / `format_memory_index`.
- Existing `format_memories` already uses `#id` — keep that wording: `Known memories (from earlier sessions):`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn render_memory_block_full_mode_under_threshold() {
    let items = vec![MemoryItem { id: 1, scope: Scope::Global, content: "likes tea".into(),
        key: None, summary: None, workspace: None, session_id: None, created_at: None }];
    let block = render_memory_block(&items);
    assert!(block.contains("[#1] likes tea"));
    assert!(!block.contains("memory_read"));
}

#[test]
fn render_memory_block_index_mode_over_threshold() {
    let items: Vec<_> = (1..=40).map(|id| MemoryItem {
        id, scope: Scope::Global,
        content: "x".repeat(300),
        key: None, summary: Some(format!("sum {id}")), workspace: None,
        session_id: None, created_at: None,
    }).collect();
    let block = render_memory_block(&items);
    assert!(block.contains("memory_read"));
    assert!(block.contains("sum 1") || block.contains("[#1]"));
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cd crates && cargo test -p ocw-data render_memory_block -- --nocapture`

- [ ] **Step 3: Implement trait + render**

- Impl `MemoryBackend` for `MemoryStore` (map infallible → `Ok`) and `SQLiteMemoryStore`.
- Port index note string from Python `_INDEX_NOTE`.
- Export `MemoryBackend`, `render_memory_block`, `INDEX_THRESHOLD_CHARS` from `lib.rs`.

- [ ] **Step 4: Run — expect PASS**

Run: `cd crates && cargo test -p ocw-data -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add crates/data/src/memory.rs crates/data/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(data): MemoryBackend trait and index-mode memory render

EOF
)"
```

---

### Task 3: Wire SQLite into `AppState` + fix prompt injection

**Files:**
- Modify: `crates/server/src/state.rs` (`AppState::new`, `format_memory_for_prompt`, `build_system_messages`, `MEMORY_GUIDANCE`)
- Modify: `crates/server/src/projects.rs` (add `format_user_rules`)

**Interfaces:**
- Consumes: `MemoryBackend`, `render_memory_block`, `SQLiteMemoryStore::open`
- Changes: `pub memory_store: Arc<dyn MemoryBackend>`
- `format_memory_for_prompt(workspace)` lists `Scope::Global` + `Scope::Workspace` with `workspace = Some(project_key)` when non-empty; empty workspace → global only
- Inject order in `build_system_messages` (after conventions, before skills):  
  1. `format_user_rules(settings.user_rules)`  
  2. `MEMORY_GUIDANCE` (expand to full Python `_MEMORY_GUIDANCE` text)  
  3. `render_memory_block(...)` if non-empty  
- Only inject memory blocks when `memory_store` is present (always is once wired)

- [ ] **Step 1: Write failing test in `state.rs` tests**

```rust
#[tokio::test]
async fn build_system_messages_injects_global_and_workspace_memory() {
    let dir = temp_data_dir("mem-inject");
    let state = make_state(dir);
    let _ = state.memory_store.add(
        "I am Alice", Scope::Global, None, None, None, None,
    ).unwrap();
    let key = crate::projects::project_key("/tmp/proj");
    let _ = state.memory_store.add(
        "uses cargo", Scope::Workspace, None, None, Some(&key), None,
    ).unwrap();
    // also poison: workspace path that should NOT match
    let _ = state.memory_store.add(
        "other project", Scope::Workspace, None, None, Some("/other"), None,
    ).unwrap();

    let msgs = state.build_system_messages("code", "/tmp/proj", "m").await;
    let text = msgs[0].content_text(); // use whatever accessor Message exposes
    assert!(text.contains("[#"));
    assert!(text.contains("Alice"));
    assert!(text.contains("uses cargo"));
    assert!(!text.contains("other project"));
}
```

(If `Message` has no helper, match on `role`/`content` fields used elsewhere in state tests.)

- [ ] **Step 2: Run — expect FAIL**

Run: `cd crates && cargo test -p ocw-server build_system_messages_injects_global -- --nocapture`

- [ ] **Step 3: Implement**

```rust
// AppState::new
let db = config.data_dir.join("coworker.db");
let memory_store: Arc<dyn MemoryBackend> = Arc::new(
    SQLiteMemoryStore::open(&db).expect("open memory db"),
);
```

Replace `format_memory_for_prompt`:

```rust
fn format_memory_for_prompt(&self, workspace: &str) -> String {
    let mut items = self.memory_store.list(Some(Scope::Global), None, None).unwrap_or_default();
    let ws = workspace.trim();
    if !ws.is_empty() {
        let key = crate::projects::project_key(ws);
        items.extend(
            self.memory_store
                .list(Some(Scope::Workspace), Some(&key), None)
                .unwrap_or_default(),
        );
    }
    render_memory_block(&items)
}
```

Copy full Python `_MEMORY_GUIDANCE` into `MEMORY_GUIDANCE`.  
Add `format_user_rules` mirroring Python; call from `build_system_messages` using `memory_settings.snapshot()["user_rules"]`.

Fix all call sites that assumed concrete `MemoryStore` (`.add` now returns `Result`).

- [ ] **Step 4: Run tests**

Run: `cd crates && cargo test -p ocw-server build_system_messages -- --nocapture`  
Expected: PASS (including existing guidance test)

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/state.rs crates/server/src/projects.rs
git commit -m "$(cat <<'EOF'
feat(server): persist memory in SQLite and inject global+workspace block

EOF
)"
```

---

### Task 4: Fix REST memory handlers

**Files:**
- Modify: `crates/server/src/app.rs` (`handler_add_memory`, optionally patch for summary)
- Test: add unit/integration test near app handlers or state — prefer a focused test that calls store the same way the handler will

**Interfaces:**
- `POST /v1/memory` body: `{ content, scope?, summary?, workspace? }`  
  - workspace: if scope is workspace and body omits workspace, leave `None` only for global; for workspace scope prefer body `workspace` then fall back to nothing (GUI may send it — check `MemorySection` / `api.ts`)
- Map `session` scope → `workspace` (Python tools coerce session → workspace)
- `PATCH` accepts optional `summary`

- [ ] **Step 1: Inspect GUI payload**

Read `surfaces/gui/src/api.ts` memory helpers and `MemorySection.tsx` — note which fields are sent on add.

- [ ] **Step 2: Write failing test**

```rust
#[test]
fn rest_add_memory_stores_workspace_key() {
    // Construct AppState, invoke the same logic as handler_add_memory
    // Assert list(Workspace, Some(project_key)) contains the item
}
```

- [ ] **Step 3: Implement handler**

```rust
async fn handler_add_memory(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("").trim();
    if content.is_empty() {
        return Json(json!({ "ok": false, "error": "content required" }));
    }
    let mut scope = match body.get("scope").and_then(|v| v.as_str()) {
        Some("global") => Scope::Global,
        Some("session") => Scope::Workspace, // dead scope
        _ => Scope::Workspace,
    };
    let summary = body.get("summary").and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty());
    let workspace = body.get("workspace").and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty());
    let ws_key = workspace.map(|w| crate::projects::project_key(w));
    let ws_arg = if matches!(scope, Scope::Workspace) { ws_key.as_deref() } else { None };
    match state.memory_store.add(content, scope, None, summary, ws_arg, None) {
        Ok(entry) => Json(serde_json::to_value(&entry).unwrap_or(json!({}))),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}
```

Update `handler_patch_memory` / `delete` / `list` / `delete_all` for `Result` APIs.

- [ ] **Step 4: Run**

Run: `cd crates && cargo test -p ocw-server rest_add_memory -- --nocapture`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/app.rs
git commit -m "$(cat <<'EOF'
fix(server): REST memory add persists workspace and summary

EOF
)"
```

---

### Task 5: Agent memory tools

**Files:**
- Create: `crates/server/src/memory_tools.rs`
- Modify: `crates/server/src/lib.rs` (`mod memory_tools;`)
- Modify: `crates/server/src/ws.rs` (`build_builtin_registry` signature + call)
- Test: `crates/server/src/memory_tools.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Arc<dyn MemoryBackend>`, `workspace: Option<String>`, `saving_enabled: Arc<dyn Fn() -> bool + Send + Sync>`
- Produces: `pub(crate) fn register_memory_tools(registry: &mut ToolRegistry, ...)`
- Tools (low risk / memory category metadata if ToolSpec supports it):
  - `remember(content, summary?, scope?)`
  - `memory_read(memory_ids: array)`
  - `memory_update(memory_id, content, summary?)`
  - `memory_forget(memory_id)`
- Off error string = Python `_OFF_ERROR` verbatim.
- `on_saved` callback optional `Arc<dyn Fn(MemoryItem, Option<String>) + Send + Sync>` — **pass no-op / None in this plan** (toast later).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn remember_tool_persists_when_saving_on() {
    let store: Arc<dyn MemoryBackend> = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::new();
    register_memory_tools(&mut reg, store.clone(), Some("/tmp/ws".into()),
        Arc::new(|| true), None);
    assert!(reg.contains("remember"));
    let result = reg.call("remember", json_map!{ "content": "likes dark mode", "scope": "global" });
    // assert saved + store.list Global contains it
}

#[test]
fn remember_tool_refuses_when_saving_off() {
    // saving_enabled = || false → saved: false, error contains "turned off"
}
```

- [ ] **Step 2: Run — expect FAIL** (module missing)

- [ ] **Step 3: Implement `memory_tools.rs`**

Mirror `coworker/memory/tools.py` and the `register_scheduling_tools` Arc-closure style.  
Register with `requires_approval: false` / low risk.

Wire into `build_builtin_registry`:

```rust
pub(crate) fn build_builtin_registry(
    ...
    memory: Option<(Arc<dyn MemoryBackend>, Option<String>, Arc<dyn Fn() -> bool + Send + Sync>)>,
) -> StdArc<ToolRegistry> {
    ...
    if let Some((store, workspace, saving)) = memory {
        crate::memory_tools::register_memory_tools(
            &mut reg, store, workspace, saving, None,
        );
    }
}
```

At every live-engine build site (search `build_builtin_registry(`), pass:

```rust
Some((
    Arc::clone(&state.memory_store),
    Some(crate::projects::project_key(&workspace)),
    {
        let settings = state.memory_settings.clone();
        Arc::new(move || {
            settings.snapshot().get("enabled").and_then(|v| v.as_bool()).unwrap_or(true)
        })
    },
))
```

For scheduled/task engines that pass `None` for automations, still pass memory (Python task engines get memory too — check `manager.py` `_build_task_engine`; if yes, wire; if no, keep `None` and document).

- [ ] **Step 4: Run**

Run: `cd crates && cargo test -p ocw-server memory_tools -- --nocapture`  
Also: `cargo test -p ocw-server --lib` to catch registry call-site compile errors.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/memory_tools.rs crates/server/src/lib.rs crates/server/src/ws.rs
git commit -m "$(cat <<'EOF'
feat(server): register remember/memory_* agent tools

EOF
)"
```

---

### Task 6: Per-turn `_MEMORY_OFF_NOTICE`

**Files:**
- Modify: `crates/server/src/state.rs` (constant `MEMORY_OFF_NOTICE`)
- Modify: `crates/server/src/ws.rs` (`with_context_provider` closures ~451 and ~1459)

**Interfaces:**
- When `memory_settings.enabled == false`, append Python `_MEMORY_OFF_NOTICE` text to the per-turn context string (alongside current date).
- Guidance stays in system prompt; tools stay registered (writes refuse).

- [ ] **Step 1: Write failing test** if context_provider is hard to unit-test, add a pure helper:

```rust
pub(crate) fn memory_turn_context(saving_enabled: bool) -> Option<&'static str> {
    if saving_enabled { None } else { Some(MEMORY_OFF_NOTICE) }
}
```

Test that helper; then use it inside `with_context_provider`.

- [ ] **Step 2–4: Implement + test + commit**

```bash
git commit -m "$(cat <<'EOF'
feat(server): inject memory off notice when saving disabled

EOF
)"
```

---

### Task 7: End-to-end sanity + parity note

**Files:**
- Modify: `docs/parity-report-2026-08-07.md` — mark N30 core as addressed (note toast still open)
- Optional: add `crates/server` test that simulates Settings add → new `build_system_messages` sees `#id`

- [ ] **Step 1: Manual checklist (run against `cargo run` / GUI if available)**

1. Settings → Memory → add a global fact → new chat → model sees it / Stop not required — check system prompt via log or `remember` not needed  
2. Ask agent to remember a preference → Settings list shows it → restart sidecar → still there  
3. Turn off “Remember new things” → ask to remember → agent refuses honestly  
4. Turn back on → remember works again in same session  

- [ ] **Step 2: Run full relevant suites**

```bash
cd crates && cargo test -p ocw-data -- --nocapture
cd crates && cargo test -p ocw-server memory -- --nocapture
cd crates && cargo test -p ocw-server build_system_messages -- --nocapture
```

- [ ] **Step 3: Commit parity note**

```bash
git add docs/parity-report-2026-08-07.md
git commit -m "$(cat <<'EOF'
docs: note N30 memory core loop wired on Rust sidecar

EOF
)"
```

---

## Self-review

| Spec requirement | Task |
|------------------|------|
| SQLite persistence | 1, 3 |
| summary + migrate | 1 |
| Trait / injectable store | 2, 3 |
| Index/full render with `#id` | 2, 3 |
| GLOBAL+WORKSPACE inject | 3 |
| user_rules inject | 3 |
| Full MEMORY_GUIDANCE | 3 |
| REST workspace fix | 4 |
| Four tools + live saving gate | 5 |
| Off notice per turn | 6 |
| Toast/Undo | **Deferred** (design out of scope) |
| binding/rekey | **Deferred** |

No TBD placeholders in task steps. Types: `Arc<dyn MemoryBackend>` throughout Tasks 3–5.

---

## Execution handoff

Plan saved to `docs/superpowers/plans/2026-09-05-rust-memory-parity.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — same session, batch with checkpoints  

Which approach?
