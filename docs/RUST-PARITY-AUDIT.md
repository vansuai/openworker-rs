# OpenWorker Rust 协议兼容性审计（现行）

- **基线**：`coworker/` Python 实现 + GUI (`surfaces/gui`) REST/WS 契约
- **对照**：`crates/` Rust workspace（`ocw-server`）
- **口径**：HTTP/WS 路由、JSON 字段、事件、权限、SQLite/JSONL、桌面 sidecar
- **修订**：2026-08-07（替换 2026-08-05 过时结论）
- **总判定**：**路由壳接近可切换，语义与交付链路未达 1:1。** 阻断生产切流的是有路由但半实现/假成功，以及 dev/prod sidecar 不一致——不是「缺 90 条路由」。

## 0. 相对 2026-08-05 审计：已推进

| 项 | 8/5 审计 | 现行（2026-08-07） |
| --- | --- | --- |
| 公开路由重合 | 32/122 | **~118/122**（Rust ~130；Python-only 4；Rust-only 12） |
| ConversationStore | 未挂载 | 已挂 `AppState` 并持久化 |
| memory `list` 列错位 | P0 实证 bug | `key=get(2), content=get(3)` 已纠正 |
| `/ws/events` | 缺失 | [`crates/server/src/events_ws.rs`](../crates/server/src/events_ws.rs) |
| Bedrock / Vertex 源文件 | 缺失 | `crates/provider/src/{bedrock,vertex}.rs` 存在 |
| ask / plan / directory emit | 永不 emit | engine 已 emit |
| Tauri `server_bin()` | 只找 `openworker-server` | 已优先找 **`ocw-server`**（dev 回退 workspace `target`） |
| Automations scheduler | 无 loop | `scheduler.rs` 30s tick + catchup |
| Rust 单测 | 7 | ~100+ `#[test]`（仍远少于 Python ~898） |

历史 8/5 全文细节已过时；以下以现行代码为准。

---

## 1. 路由面

| | Count |
| --- | --- |
| Python（`coworker/server/app.py` `@app.*`） | **122** |
| Rust（`crates/server/src/app.rs` `.route`；WS upgrade 计为 WS） | **~130** |
| 归一化重合 | **~118** |
| Python-only | **4** |
| Rust-only | **12** |

### 1.1 Python-only

| Method | Path | 影响 |
| --- | --- | --- |
| GET | `/v1/connectors/slack/status` | GUI `api.ts` 硬编码 → Rust **404** |
| GET | `/v1/connectors/github/status` | 同上 |
| POST | `/v1/connectors/github/installations/{id}/disconnect` | `cloud::github_disconnect_installation` 有逻辑未挂路由 |
| POST | `/v1/_debug/inject_inbound` | debug only |

### 1.2 Rust-only（超集，不算回归）

- `POST/GET /v1/sessions`、`GET/POST /v1/sessions/{id}/skills`
- Skills 全 CRUD / upload / move（Python 仅 `GET /v1/skills` + 文件系统）
- `GET /v1/connectors/{name}/status`（通用路径；**不能**替代 GUI 的 slack/github 专用 URL）

### 1.3 子系统语义状态

| 子系统 | 状态 | 说明 |
| --- | --- | --- |
| health / settings / providers 元数据 / memory / chat / audit / channels / messaging / subscriptions / web-search / attachments | Equivalent | 真实读写 |
| sessions | Equivalent | 含持久化；Rust 额外 create/get/skills |
| automations | Partial | CRUD + scheduler loop；**五段 cron 解析失败 → now+60** |
| agents | Partial | 形状 ≠ Python `sidebar()` |
| personas | Partial | CRUD 有；disable 不 archive |
| workspaces | Partial | recent/open/trust 真；`/pick` 恒失败（依赖 GUI 原生选目录） |
| connectors | Partial | 账户/目录多数有；专用 status 404；mcp-connect 假成功；**无 inbound Gateway** |
| inbox | Partial | store 有；**resolve 不 durable resume** |
| mcp / oauth | Partial / Broken | mcp.json CRUD 真；tools `[]`；connect 假成功；OAuth stub |
| browser | Missing | 无 Playwright |
| ws `/ws/session` | Partial | turn/approval 有；整 turn 后刷事件；无 MCP tools；Stop 未接通 |
| ws `/ws/events` | Equivalent | broadcast 存在 |
| TUI CLI | Missing | 仍仅 Python Textual |
| 打包生产 | Broken | stage Python `openworker-server`，Tauri 找 `ocw-server` |

---

## 2. P0–P3 清单（现行）

### P0 — 切流阻断

1. **打包 sidecar 与 Tauri 不一致** — `build_dmg.sh` / `build_windows.ps1` stage PyInstaller `openworker-server`；Tauri 找 `ocw-server`。
2. **无端到端真流式** — `Router::stream` 走 `complete()`；WS 整 turn 后再广播。
3. **Stop / interrupt 无效** — WS `ctx.cancel` 与 engine 本地 cancel 未共享。
4. **MCP 运行时假实现** — `tools: []`；connect 直接 `connected: true`；OAuth 无 pending；WS 不注册 MCP tools。
5. **五段 cron → now+60** — Python `croniter` 五段；Rust `cron` 要六段；失败回落 `now + 60`。
6. **Inbox resolve 不 resume** — 无 Python `_durable_resume`。

### P1 — GUI 可感知缺口

7. Slack/GitHub 专用 status 路径缺失。  
8. GitHub installation disconnect 未挂路由。  
9. `GET /v1/agents` 响应形状错误。  
10. 无 connectors Gateway（Slack Socket Mode / Telegram / Email 入站）。  
11. Browser automation 不可用。  
12. connector `mcp-connect` 假 `started: true`。

### P2 — 体验 / 边界

13. Personas disable 不 archive。  
14. Skills REST 为 Rust 超集（需文档）。  
15. TUI 未迁移 — **决策：桌面 + HTTP 为一等公民；TUI 过渡期保留 Python `openworker` CLI。**  
16. CI 无 `cargo check/test`；无 Python↔Rust 契约门禁。  
17. README / `setup_dev_env.sh` 仍主推 Python server。

### P3 — 测试 / 文档

18. Rust 测试远少于 Python；无系统化契约差分。  
19. GUI e2e 默认仍对 Python 后端。

---

## 3. GUI 调用矩阵（要点）

`surfaces/gui/src/api.ts` 硬编码且会砸体验的路径：

- `GET /v1/connectors/github/status`、`GET /v1/connectors/slack/status`
- `disconnectGithubInstallation` → `POST .../github/installations/{id}/disconnect`
- 其余 `/v1/*` 在 Rust 路由面上大多已注册；风险转为 **假成功 / 空 tools / 错误 JSON 形状**，而非 404。

---

## 4. 切流策略与验收

**策略**：分阶段双轨 — GUI 核心等价后打包默认 `ocw-server`，保留 `COWORKER_SERVER_BIN` / `OCW_SIDECAR=python` 回退；再清 Gateway / Browser / MCP OAuth。

| 门禁 | 标准 |
| --- | --- |
| Phase C 切流 | P0 项绿；打包 stage `ocw-server`；核心 GUI 无 404/假成功；流式与 Stop 可感一致 |
| Phase D 完成 | 公开能力在 Rust 复现或文档明示废弃；默认发布可去 Python sidecar |

## 5. 优先改动文件

- [`crates/data/src/automation.rs`](../crates/data/src/automation.rs) — cron  
- [`crates/provider/src/router.rs`](../crates/provider/src/router.rs)、[`crates/engine/src/engine.rs`](../crates/engine/src/engine.rs)、[`crates/server/src/ws.rs`](../crates/server/src/ws.rs) — 流式 / Stop  
- [`crates/server/src/subsystems.rs`](../crates/server/src/subsystems.rs)、[`crates/server/src/mcp.rs`](../crates/server/src/mcp.rs)、[`crates/server/src/app.rs`](../crates/server/src/app.rs) — MCP / 契约  
- [`packaging/build_dmg.sh`](../packaging/build_dmg.sh)、[`packaging/build_windows.ps1`](../packaging/build_windows.ps1) — sidecar  
- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) — cargo 门禁  

## 6. Phase D 决策（2026-08-07）

| 主题 | 决策 |
| --- | --- |
| **TUI** | 桌面 GUI + HTTP/`ocw-server` 为一等公民。Textual TUI（`openworker` CLI）过渡期保留 Python；不阻塞切流。 |
| **Browser** | Rust `BrowserController` 继续诚实返回 unavailable；Playwright 端口列入后续迭代，禁止假截图成功。 |
| **Gateway inbound** | [`crates/connectors/src/gateway.rs`](../crates/connectors/src/gateway.rs) 提供生命周期壳；`start()` 返回空列表；Slack/Telegram/Email 入站仍未移植。 |
| **MCP OAuth** | stdio/HTTP tools/list 已接线；connector managed OAuth MCP-connect 返回明确错误（非 `started:true`）。 |
| **打包回退** | 默认 stage `ocw-server`；`OCW_SIDECAR=python` 或 `COWORKER_SERVER_BIN` 可回退。 |
| **CI** | `cargo check/test --workspace` + `scripts/rust_route_parity.py` 进入 CI。 |
