# OpenWorker Rust 协议兼容性审计（现行）

- **产品目标**：Rust `crates/`（`ocw-server`）为唯一运行时与发布路径
- **Python 角色**：`coworker/` **仅作迁移对照参考**（行为/契约/测试的真相源），不再作为产品后端长期维护
- **GUI 契约**：`surfaces/gui` REST/WS 仍以与 Python 基线 1:1 等价为验收口径
- **口径**：HTTP/WS 路由、JSON 字段、事件、权限、SQLite/JSONL、桌面 sidecar
- **修订**：2026-09-05（相对上游 `andrewyng/openworker` `main` @ `5bc10d9` 全量同步后更新；此前 2026-08-07 源码级复核见 [parity-report-2026-08-07.md](parity-report-2026-08-07.md)）
- **上游基线**：Python 参考已覆盖至 `5bc10d9`（含 OPE-136 MCP 权限、compaction/reviewer/provenance/teams、security personas）
- **总判定**：**Python 参考与核心安全契约已对齐上游；Rust 产品路径完成 Egress/MCP floor、审批 grant、conversation 健壮性、Anthropic stream-complete、skills staging、inbox 句首 intent、board API 骨架与 GUI 审批卡/i18n/MCP trust。** HTTP 路由面已与 Python 基本重合（Python-only 仅 debug）；仍有深度接线缺口（见 §7）。

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
| Python（`coworker/server/app.py` `@app.*`） | **182**（`scripts/rust_route_parity.py`，2026-09-05） |
| Rust（`crates/server/src/*.rs` `.route`） | **185** |
| 归一化重合 | **181** |
| Python-only | **1** |
| Rust-only | **4** |

### 1.1 Python-only（2026-09-05 路由对齐后）

| Method | Path | 影响 |
| --- | --- | --- |
| POST | `/v1/_debug/inject_inbound` | debug only（有意跳过） |

> 2026-09-05 已补齐：settings（auto-approve / compaction / context-bar）、Codex status/signin/signout、token board 突变、memory CRUD/settings、project bindings/menu、persona media/export、skills reveal、temp workspace、reviewer-stats。

### 1.2 Rust-only（超集，不算回归）

- `POST/GET /v1/sessions`、`GET/POST /v1/sessions/{id}/skills` 等会话/技能超集
- Skills 全 CRUD / upload / move（相对 Python 文件系统侧）
- 其它产品路径扩展路由（见 `scripts/rust_route_parity.py` Rust-only 列表）

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
| TUI CLI | Missing | 参考实现仍在 Python Textual；产品路径为 GUI/`ocw-server` |
| 打包生产 | Equivalent（默认） | 默认 stage `ocw-server`；`OCW_SIDECAR=python` 仅临时对照/应急 |

---

## 2. P0–P3 清单（现行，2026-08-07 源码复核后修订）

> 复核详情与全部证据见 [parity-report-2026-08-07.md](parity-report-2026-08-07.md)；状态标注：已修复 / 部分推翻（降级） / 确认。

### P0 — 切流阻断

1. ~~**打包 sidecar 与 Tauri 不一致**~~ — **已修复**：build_dmg.sh / build_windows.ps1 默认 stage `ocw-server`，与 Tauri `server_bin()` 一致。
2. **无端到端真流式** — **部分推翻**：WS live pump 已实时广播；但 provider 层仍全缓冲（openai/anthropic/gemini `resp.text()` 整读后重放；bedrock/vertex 无 stream）。
3. **Stop / interrupt 无效** — **部分推翻**：cancel 链路已共享；残留：审批等待不响应停止、无 interrupt_hooks 杀 shell。
4. **MCP 运行时假实现** — **改判**：tools/list 真实、connect 诚实；真缺口为 **tools/call 零路径（会话中不可执行）** 与 OAuth stub。
5. ~~**五段 cron → now+60**~~ — **已修复**（补秒位 + 回归测试）；残留：timezone 恒 UTC、schedule_human dow 取错字段。
6. **Inbox resolve 不 resume** — **确认**，叠加无 mid-turn checkpoint。
7. **（新）安全回归**：无 HTTP 鉴权中间件、CorsLayer::permissive()、WS 鉴权退化且打印 token 明文、Origin starts_with 可绕过、/ws/events 无鉴权。
8. **（新）权限管道断裂**：PermissionEngine workspace_root 传入 permissions.json 路径 → 写操作硬拒；会话 mode 不生效；plan 批准不翻转 mode；allowlist/task_rules 零注入。
9. **（新）Provider 构造错误**：Anthropic system 形状非法/消息折叠死代码/图片键名错；Gemini 工具回放断裂；Bedrock 无 SigV4；Vertex region 字段不匹配；凭据 env 回退被短路；verify 为无条件 ok:true stub。

### P1 — GUI 可感知缺口

7'. ~~Slack/GitHub 专用 status 路径缺失~~ / 8'. ~~GitHub disconnect 未挂路由~~ / 9'. ~~GET /v1/agents 形状错误~~ / 12'. ~~mcp-connect 假成功~~ — **均已修复**。
10. **无 connectors Gateway** — **确认，且更严重**：gateway.rs start() 恒空且未挂 AppState，六条入站链路（allowlist 执行、reply-token、interaction、parked 重注入、mention、mirror）无运行时载体。
11. **Browser automation 不可用** — 确认（诚实 unavailable，决策项）。
13'. **（新）WS 层**：限流时钟 bug（30 条后永久拒绝）；question/directory/plan 不经 inbox；多视图被顶掉；nav-layout 参数名不兼容；token usage/工具消息不落盘。
14'. **（新）数据/工具层**：Memory 三连锁断（内存版存储 + 键不匹配 + 无工具）；load_skill 未注册但 prompt 引用；persona 运行时不生效；send_message/attribution/scheduling/selfwake 等工具缺失。

### P2 — 体验 / 边界

13. Personas disable 不 archive。  
14. Skills REST 为 Rust 超集（需文档）。  
15. TUI 未移植 — **决策：不作为产品路径；需要时对照 `coworker/tui` 再迁，或废弃。**  
16. CI 已加 `cargo check/test` + 路由探针；Python pytest 保留作参考回归。  
17. README / `setup_dev_env.sh` 以 `ocw-server` 为主路径。

### P3 — 测试 / 文档

18. Rust 测试仍少于 Python 参考套件；契约差分以 `scripts/rust_route_parity.py` 起步。  
19. GUI e2e 应对准 Rust 后端（参考 Python 仅用于行为对照）。

---

## 3. GUI 调用矩阵（要点）

`surfaces/gui/src/api.ts` 硬编码且会砸体验的路径（相对参考实现）：

- `GET /v1/connectors/github/status`、`GET /v1/connectors/slack/status`（Rust 已补）
- `disconnectGithubInstallation` → `POST .../github/installations/{id}/disconnect`（Rust 已补）
- 其余风险主要是 **假成功 / 空 tools / 错误 JSON 形状**，而非 404。

---

## 4. 切流策略与验收

**策略**：Rust 为唯一产品路径。`coworker/` 只读对照；打包默认 `ocw-server`。`OCW_SIDECAR=python` / `COWORKER_SERVER_BIN` 仅用于迁移期对照或紧急回滚，**不是并行产品线**。

| 门禁 | 标准 |
| --- | --- |
| 切流可用 | P0 项绿；默认打包 `ocw-server`；核心 GUI 无 404/假成功；流式与 Stop 可感一致 |
| 迁移完成 | 参考实现中的公开能力均在 Rust 复现（或文档明示废弃）；可移除 Python sidecar 与发布依赖 |

## 5. 优先改动文件

- [`crates/data/src/automation.rs`](../crates/data/src/automation.rs) — cron  
- [`crates/provider/src/router.rs`](../crates/provider/src/router.rs)、[`crates/engine/src/engine.rs`](../crates/engine/src/engine.rs)、[`crates/server/src/ws.rs`](../crates/server/src/ws.rs) — 流式 / Stop  
- [`crates/server/src/subsystems.rs`](../crates/server/src/subsystems.rs)、[`crates/server/src/mcp.rs`](../crates/server/src/mcp.rs)、[`crates/server/src/app.rs`](../crates/server/src/app.rs) — MCP / 契约  
- [`packaging/build_dmg.sh`](../packaging/build_dmg.sh)、[`packaging/build_windows.ps1`](../packaging/build_windows.ps1) — sidecar  
- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) — cargo 门禁  

## 6. Phase D 决策（2026-08-07）

| 主题 | 决策 |
| --- | --- |
| **Python 代码** | **仅迁移参考**；不以 Python 为发布/运行时产品。 |
| **TUI** | 非产品一等路径；对照 `coworker/tui` 可选后迁或废弃。 |
| **Browser** | Rust `BrowserController` 诚实返回 unavailable；对照 Python Playwright 后续移植。 |
| **Gateway inbound** | [`crates/connectors/src/gateway.rs`](../crates/connectors/src/gateway.rs) 生命周期壳；入站对照 Python gateway 继续移植。 |
| **MCP OAuth** | stdio/HTTP tools/list 已接线；managed OAuth 对照 Python 后续补齐。 |
| **打包** | 默认 `ocw-server`；Python sidecar 仅临时对照/应急。 |
| **CI** | `cargo check/test --workspace` + `scripts/rust_route_parity.py`；pytest 为参考回归。 |

## 7. 相对 upstream `5bc10d9`（2026-09-05）

### 7.1 已同步

| 区域 | 状态 |
| --- | --- |
| Python `coworker/` + `tests/` + `pyproject.toml` + `SECURITY.md` | 按上游覆盖；`pytest` ~1911 passed（排除本机 DNS 噪声的 URL guard） |
| Risk / MCP floor / `RiskClass::Egress` | [`permissions.rs`](../crates/engine/src/permissions.rs)；override 只可收紧 |
| 审批 grant（once / this-run / always-trust / deny） | engine + WS resolution 词汇对齐 |
| Conversation 健壮性 | 坏行跳过、原子 shrink、拒绝路径穿越 session id、trailing pending 不填假 result |
| Anthropic `complete()` | 内部 stream 再累积 |
| Skills upload staging 路径约束 | [`skills/store.rs`](../crates/skills/src/store.rs) |
| Inbox reply 句首 intent | [`inbox_routing.rs`](../crates/data/src/inbox_routing.rs) |
| MCP trust / revoke / convert API | server routes + GUI `CustomMcp` / `ToolReview` |
| GUI OPE-136 审批卡 + i18n en/zh | cherry-pick；保留 Tauri/`ocw-server` 接线 |
| Compaction / reviewer / provenance | **已挂入 TurnEngine**（due→summarize、overflow 重试、`apply_to_outbound`、SessionFiles、`_display` origin、AutoApprove 评审） |
| Teams/Board HTTP | `/v1/board/*`（token 鉴权）+ GUI Board 面板 / Sidebar 入口 |
| Codex / OpenAI Responses | **已实现并接入 router**（stock OpenAI → Responses；`openai-codex` 描述符） |
| Security personas | Python builtin 资源已随覆盖带入 |
| Scheduler #379 | claim-at-dispatch + catchup 后 `sleep(30)` 再 schedule（对齐 Python，避免 interval 立即双跑） |

### 7.2 仍存缺口（相对上游行为契约）

| 缺口 | 说明 |
| --- | --- |
| HTTP 路由面 | **已对齐**：Python-only 仅 `_debug/inject_inbound`；settings/codex/board/memory/projects/misc 已挂 `ocw-server` |
| Board journal | token `/v1/board/journal*` 仍为 **stub**（空 cases/entries） |
| Codex OAuth 浏览器流 | status/signout **真**；signin 诚实 stub（可读 secrets 里已有 token） |
| reviewer-stats | 形状对齐，计量暂为 zeros stub（未接 audit 聚合） |
| `ocw` CLI / board MCP | 可后置 |
| RightRail 内 BoardSection 深度集成 | 组件已有；App session 右栏挂载可继续打磨 |
| Team chat | HTTP stub（`enabled:false`） |

> 2026-09-05 补丁：TurnEngine 已挂 compaction/reviewer/provenance；WS 仅在 `auto_approve` 时挂 reviewer，并注入 live compaction settings；stock OpenAI / `openai-codex` 走 Responses；scheduler catchup 后 sleep 再 tick；settings/board/memory/project 路由面与 Python 重合。

### 7.3 验收命令（2026-09-05）

```text
.venv/bin/python -m pytest tests -q --ignore=tests/test_url_address_guard.py
# → ~1911 passed

cd crates && cargo test --workspace --lib
# → 各 crate ok（engine 38、provider 含 Responses 单测、ocw-server 等）

cd surfaces/gui && npx tsc --noEmit && npx vitest run
# → tsc 0；143 tests passed
```
