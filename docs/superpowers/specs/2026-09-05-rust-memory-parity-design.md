# Rust Memory Parity — Design

**Status:** proposed  
**Scope:** core closed loop for Tauri/Rust sidecar (N30). Toast/Undo deferred.

## Problem

Python memory works end-to-end. Rust has REST + Settings UI + prompt text mentioning `remember`, but:

- production uses in-memory `MemoryStore` (restart loses data; `SQLiteMemoryStore` unused)
- no agent tools (`remember` / `memory_read` / `memory_update` / `memory_forget`)
- REST `add` writes `workspace=None` while prompt filters by workspace → Settings entries never inject
- no `user_rules` injection; `enabled` does not gate writes; no `_MEMORY_OFF_NOTICE`

## Goal

In a GUI session via Rust sidecar: agent can save/read/update/forget memories; they persist across restarts; they inject into new sessions; Settings toggle stops learning (not amnesia).

## Approaches

| Approach | Pros | Cons |
|----------|------|------|
| **A. Trait + SQLite production** | Matches Python adapter pattern; tests keep Vec store | Touch AppState type |
| B. SQLite only everywhere | Simpler types | Tests need temp files / `:memory:` |
| C. Fix REST only + keep Vec | Tiny | Still no tools / no persist — does not meet goal |

**Recommendation: A.** Introduce `MemoryBackend` trait in `ocw_data`; `AppState` holds `Arc<dyn MemoryBackend>`; production opens `data_dir/coworker.db`.

## Behavior (mirror `agent.py` + `tests/test_memory*.py`, not outdated settings docstring)

- Off = stop **learning**: writes refuse with honesty error; known memories still inject; tools stay registered; per-turn off notice
- Knowledge frozen at engine build (GLOBAL + WORKSPACE@key); deletions apply to **new** sessions
- Workspace identity for this plan: `projects::project_key(workspace)` (bindings/rekey = follow-up)
- Index mode when rendered block > 8000 chars (newest 10 full)
- `summary` on items; session scope coerced to workspace on save

## Out of scope (follow-ups)

- WS `memory_saved` toast + GUI Undo
- `resolve_memory_key` binding ladder + `rekey_workspace`
- GUI `announceMemoryChanged` live refresh

## Success criteria

1. Restart app → memories still in Settings list and in new session prompts  
2. Agent `remember` → same store Settings reads  
3. Settings add with workspace → appears in that workspace’s prompt  
4. Toggle saving off mid-session → next write refuses; known list still present  
5. Unit tests cover store, render, tools, injection list composition  
