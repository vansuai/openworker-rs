# OpenWorker Rust 还原度详细核验报告（源码对照）

- 日期：2026-08-07
- 方法：逐子系统源码对照（Python `coworker/` 参考实现 vs Rust `crates/`），不依赖动态差分
- 基线：`cargo test --workspace` 116/116 通过；`pytest tests -q`（Python 3.12）**947 passed / 1 skipped / 0 failed**
- 配套：本报告细化并修正 [RUST-PARITY-AUDIT.md](RUST-PARITY-AUDIT.md) 的结论

---

## 1. 总判定

**路由壳与数据存储层高保真；执行语义层多处断链。**

| 维度 | 结论 |
| --- | --- |
| 路由注册 | 122 条 Python 路由中 121 条已在 Rust 注册（仅缺 debug 路由），Rust 另有 12 条超集 |
| 数据存储层 | SQLite/JSONL 表结构、inbox 状态机、routing 配置、accounts、directory、secrets、trust、skills 存储均真实且高保真 |
| 执行语义层 | 引擎可靠性语义（retry/durable resume/中断收尾/plan 批准续跑/mode 生效）、权限数据管道、provider 正确性、入站编排、安全契约存在系统性断链 |
| 安全契约 | **高危回归**：HTTP 鉴权归零、CORS 全开、WS 鉴权退化并泄漏 token |

子系统分档计数（共 24 个子项）：Equivalent 8 / Partial 9 / Broken 4 / Missing 3。

---

## 2. 路由面全量对账表（Python 122 条）

状态定义：对齐 = handler 存在且语义等价；契约差 = 有 handler 但参数/响应/错误码与 Python 不一致；stub = 恒成功/恒失败假实现；缺失 = 404。

### 2.1 基础与设置（18 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/health | 对齐 | Rust 多返回字段（超集） |
| GET /v1/settings | 对齐 | |
| POST /v1/settings/model-key | 对齐 | |
| POST /v1/settings/default-model | 对齐 | |
| POST /v1/settings/models/add | 对齐 | |
| POST /v1/settings/models/remove | 对齐 | |
| POST /v1/settings/onboarded | 对齐 | |
| POST /v1/settings/pdf | 对齐 | |
| POST /v1/settings/scratch-base | 对齐 | |
| POST /v1/settings/sessions-peek | 对齐 | |
| POST /v1/settings/surfaces | 对齐 | |
| POST /v1/settings/experimental-connectors | 对齐 | |
| POST /v1/settings/nav-layout | **契约差** | Rust 读 `layout`，Python/GUI 发 `nav_layout`（settings.rs L259-276）→ 静默重置为 flat |
| GET /v1/audit | 对齐 | |
| GET /v1/unrouted | 对齐 | |
| GET /v1/subscriptions / POST / POST remove | 对齐 | agent 侧订阅工具缺失（见 N32） |
| GET /v1/channels/recent | 对齐 | |

### 2.2 Providers / Chat（5 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/providers | 对齐 | |
| POST /v1/providers | 对齐 | profile 存储；但运行时 env 回退被短路（N27） |
| DELETE /v1/providers/{name} | 对齐 | |
| POST /v1/providers/verify | **stub** | 无条件 `{"ok":true}`（settings.rs L287-292），坏 key 也通过 |
| POST /v1/chat/completions | 契约差 | stream 恒 false、temperature 默认 0.7（Python 不注入） |

### 2.3 Sessions（14 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/sessions | **契约差** | 响应字段缩减：无 attention/liveness/origin/origin_label，不过滤 `__` 内部会话 |
| PATCH /v1/sessions/{id} | 对齐 | |
| DELETE /v1/sessions/{id} | **契约差** | 无级联清理（subscriptions/inbox/mentions/scratch 目录残留） |
| GET .../{id}/messages | 对齐 | 但持久化字段缩水（N14）影响回放 |
| GET/POST/DELETE .../{id}/roots | 对齐 | |
| GET/POST .../{id}/connections | 对齐 | |
| GET/POST .../{id}/unattended | 对齐 | |
| GET .../{id}/artifacts / artifacts/read / artifacts/reveal | 对齐 | |

### 2.4 Automations（7 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/automations / {id} | 对齐 | |
| POST /v1/automations | **契约差** | cron 仅弱校验（5 个非空字段）；失败返回 HTTP 400 而非 200+ok:false；无 grant_entries 权限过滤、无 scratch 预配 |
| PATCH /v1/automations/{id} | 契约差 | 无 `revoke` 支持 |
| DELETE /v1/automations/{id} | 对齐 | |
| POST .../{id}/run | 契约差 | run prompt 缺「勿重复创建调度」防呆句 |
| POST .../{id}/runs/{run_id}/finalize | 契约差 | status/error 取自请求体；status 词汇 `"success"` vs Python `"ok"` |
| POST .../{id}/seen | 对齐 | |

### 2.5 Inbox / Messaging（7 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/inbox | **契约差** | 无 visibility 推导、无 session 富化字段、无孤儿清理 |
| POST /v1/inbox/{id}/resolve | **断链** | 仅广播事件，无 durable resume（审计 P0#6 确认） |
| GET /v1/inbox/routing / POST binding / GET reconcile | 对齐 | 配置面真实，数据面无消费者（mirror/deliver 缺失） |
| GET/POST /v1/messaging/dm-route | 对齐 | 仅存配置，无入站分发循环 |

### 2.6 MCP（8 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET/POST/PATCH/DELETE /v1/mcp 系列 | 对齐 | mcp.json CRUD 真实；workspace 级合并未做 |
| GET /v1/mcp/{name}/tools | 对齐 | 真实 tools/list（stdio/HTTP） |
| POST /v1/mcp/{name}/connect | 对齐 | 真实连接并返回工具数；OAuth 明确报错（诚实） |
| POST /v1/mcp/{name}/signout | stub 收尾 | 删 secrets profile，但从未有流程写入 token |
| POST /v1/mcp/reload | 对齐 | |

### 2.7 Connectors（33 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET /v1/connectors | 对齐 | |
| GET /v1/connectors/slack/status | 对齐 | 诚实报 offline（未移植） |
| GET /v1/connectors/github/status | 对齐 | 走 status 枚举，真实 |
| POST/GET connect / connect-managed / disconnect / allow / disallow | 对齐 | cloud.rs managed 流程真实 |
| POST /v1/connectors/{name}/mcp-connect | 对齐 | 诚实失败（原审计 P1#12 已修复） |
| GET slack workspaces/{team_id}/channels / directory | 对齐 | slack_directory.rs 逐函数镜像 |
| POST slack workspaces/{team_id}/disconnect | 对齐 | |
| POST slack approval-owners/add / remove | 对齐 | |
| gmail/gcal/hubspot accounts default/disconnect、gmail filters、hubspot hidden-fields | 对齐 | connector_accounts.rs 完整 |
| POST github installations/{id}/disconnect | 对齐 | cloud.rs 逻辑已挂路由 |
| POST {name}/accounts/{id}/default / disconnect | 对齐 | |
| PATCH {name}/tools | 对齐 | |
| POST {name}/unauthorized/{item_id} | **半实现** | 只更新 allowlist，parked 消息不重注入（gateway 未接线） |

### 2.8 Personas / Memory / Skills / Workspaces / 其余（28 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| GET/POST /v1/personas、GET/POST/DELETE /{id}、install、connections | 对齐 | 存储/安装真实 |
| POST /v1/personas/{id}/enable | **契约差** | 禁用不归档会话，`archived_sessions` 恒 0 |
| GET/POST /v1/memory | **断链** | 写内存版 store 且 workspace=None，永不注入 prompt（N28） |
| GET /v1/skills | 对齐 | Rust 另有全 CRUD 超集（12 条 Rust-only 路由） |
| GET /v1/workspaces/recent / trusted、POST open / trust | 对齐 | |
| POST /v1/workspaces/pick | **stub** | 恒失败，无原生对话框 |
| GET /v1/browser/state、POST close / screenshot | 诚实降级 | 明确报 unavailable（审计决策项） |
| GET/POST /v1/web-search | 契约差 | 恒 DDG，缺 provider 切换配置 |
| GET /v1/cloud/status / gallery / gallery/{slug}、POST login / logout / telemetry | 对齐 | cloud.rs 1070 行真实移植 |
| GET /auth/callback、GET/POST oauth/callback、GET /mcp/oauth/callback | 对齐 | cloud OAuth 真实；MCP OAuth 为 stub |
| POST /v1/attachments/inspect-pdf | 对齐 | lopdf 镜像 pdf_support |
| GET /v1/agents | 对齐 | 形状与 Python sidebar() 逐字段一致（审计 P1#9 已修复） |
| POST /v1/_debug/inject_inbound | **缺失** | 唯一 Python-only 路由，仅 debug 用途 |

### 2.9 WebSocket（2 条）

| 路由 | 状态 | 备注 |
| --- | --- | --- |
| WS /ws/session/{id} | **部分断链** | 消息类型全集对齐；但限流 bug（N18）、question/directory/plan 不经 inbox（N19）、无 mid-turn checkpoint（N16）、多视图被顶掉（N20） |
| WS /ws/events | **契约差** | 广播存在，但无鉴权无 Origin 检查（Python 两者皆有） |

---

## 3. 子系统语义矩阵

| 子系统 | 状态 | 核心证据 |
| --- | --- | --- |
| 引擎主循环 | Partial | 结构镜像；max_iterations 时 Rust 多发占位消息 |
| 工具调用执行 | Partial | 并行中断留孤儿 tool_call；`_display` 不落盘；error_type 缺失 |
| 中断/取消 | Partial | cancel 检查点已接通；缺 interrupt_hooks（shell 不杀）、审批等待不响应停止、无 interrupted notice |
| plan 模式 | **Broken** | 批准不翻转 permissions.mode，「approve 后续跑」失效（engine.rs L921-969） |
| token 统计 | **Broken** | usage 从不落盘（types.rs `Message::assistant` usage: None），GUI 重建恒 0 |
| autotitle | Equivalent | 逐条镜像 |
| retry | **Broken** | 引擎从不产生 error notice → retry 守卫恒拒绝 |
| durable resume | **Missing** | 无 `resume()`；引擎重建只载入 system messages（ws.rs L926-934） |
| steering / 动态上下文 | Missing | queue_steering、`<system-context>` 注入、PDF/图片按能力适配均未实现 |
| 会话持久化 | Partial | SQLite 层等价；JSONL 字段缩水（无 ts/source/usage/reasoning/tool_calls/tool 消息/notice） |
| 权限 mode 生效 | **Broken** | 会话 mode 从不应用到 PermissionEngine（恒 Interactive） |
| risk 分级 | Partial | 默认分类相反（unknown→EXTERNAL vs Python READ）；run_shell 不在 Exec 名单；写路径 scope 根传错（N6） |
| standing approvals | Partial | set_allowed_commands/set_task_rules 零调用；extract_target 字面量匹配 bug |
| risk overrides | **Missing** | 整体未实现 |
| Provider × 5 | Partial→Broken | 见 §5 N22-N27；五家均无真流式，四家工具循环结构性断裂 |
| 错误映射 | Partial | 仅 401/403/429/网络；quota/内容过滤/model-not-found 与友好文案缺失 |
| HTTP/WS 服务器 | Partial | 路由齐全；契约差异见 §2 |
| 安全契约 | **Broken** | 见 §5 N1-N5 |
| Connectors 存储/出站函数 | Equivalent | senders/accounts/directory/cloud 高保真 |
| Connectors 入站编排 | **Missing** | Gateway 未挂载，六条入站链路无运行时载体 |
| Inbox 状态机 | Equivalent | add/wait/resolve/reconcile 全对齐；resolve 触发路径仅 1/3 |
| MCP | Partial | CRUD+tools/list 真实；tools/call 零路径、OAuth stub |
| Automations | Partial | cron 正确；timezone 忽略、dow bug、overlap guard/scheduling 工具/selfwake 缺失 |
| Memory / Skills 运行时 | Broken / Partial | Memory 三连锁断；load_skill 未注册但 prompt 引用 |
| Personas 运行时 | Partial | 三方 persona 的 manifest 不进会话（get_agent 硬编码 4 agent） |
| browser / TUI | Missing（决策项） | 诚实 unavailable / 未移植 |

---

## 4. 审计 P0/P1 逐项终审

### P0（切流阻断）

| # | 审计结论 | 终审 | 证据 |
| --- | --- | --- | --- |
| P0-1 | 打包 sidecar 与 Tauri 不一致 | **已修复** | build_dmg.sh L73-74、build_windows.ps1 L66-68 默认构建 ocw-server；Tauri server_bin() 优先 ocw-server（lib.rs L58-70） |
| P0-2 | 无端到端真流式 | **部分推翻，降级** | Router::stream 已委托 client.stream()（router.rs L181-193），WS live pump 实时广播（ws.rs L968-978）；**但** provider 层全缓冲：openai.rs L233-237 / anthropic.rs L575-579 / gemini.rs L323 先 `resp.text()` 整读；bedrock/vertex 无 fn stream。首 token 时延无收益 |
| P0-3 | Stop/interrupt 无效 | **部分推翻，降级** | ctx.cancel 与 engine 已共享（ws.rs L935/959），流式循环/工具前检查点齐全；**残留**：审批/提问等待是 3600s 硬 timeout 不与 cancel 竞速（engine.rs L597-633）、无 interrupt_hooks 杀 shell、中断不追加 notice |
| P0-4 | MCP 运行时假实现 | **部分推翻，改判** | tools/list 真实（stdio/HTTP JSON-RPC 全生命周期）、connect 诚实；**真缺口**：tools/call 全 crates 零命中 → 工具列出不可执行；OAuth 为 stub |
| P0-5 | 五段 cron → now+60 | **已修复** | automations.rs L317 补秒位；data/automation.rs L446 回归测试 `five_field_cron_is_not_now_plus_60`；**残留**：timezone 恒 UTC（L320）、schedule_human dow 取 parts[3]（month 位） |
| P0-6 | Inbox resolve 不 resume | **确认** | subsystems.rs L474-514 仅广播事件，注释自认待实现；叠加无 mid-turn checkpoint → 重启后挂起工作流不可恢复 |

### P1（GUI 可感知缺口）

| # | 审计结论 | 终审 | 证据 |
| --- | --- | --- | --- |
| P1-7 | Slack/GitHub 专用 status 缺失 | **已修复**（审计 §1.1 与 §3 自相矛盾，以 §3 为准） | app.rs L318-322 已注册；relay 状态诚实 offline |
| P1-8 | GitHub disconnect 未挂路由 | **已修复** | app.rs L326-327 |
| P1-9 | GET /v1/agents 形状错误 | **已修复** | subsystems.rs handler_agents 与 Python sidebar() 逐字段一致 |
| P1-10 | 无 connectors Gateway | **确认，且更严重** | gateway.rs start() 恒空（L77-80）且 Gateway 未挂入 AppState；allowlist 执行、reply-token resolve、interaction、parked 重注入、mention 路由、inbox mirror 六条链路全部无运行时载体；Telegram run_loop 代码存在但无人启动 |
| P1-11 | Browser 不可用 | **确认（决策项）** | stores.rs L708-746 诚实 unavailable，无假成功 |
| P1-12 | mcp-connect 假 started:true | **已修复** | subsystems.rs L1679-1694 诚实返回 ok:false |

---

## 5. 新发现差异清单（按严重度定级）

### 5.1 安全类（新 P0，审计未覆盖）

| # | 问题 | 证据 |
| --- | --- | --- |
| N1 | **无 HTTP 鉴权中间件**：122 条 REST 路由裸奔。Python 有 x-openworker-token 中间件且默认自动生成 token（fail-closed）；Rust api_token 仅存 state 无执行点（fail-open） | app.rs L431 唯一 layer 为 CorsLayer；state.rs L87 |
| N2 | **CorsLayer::permissive()**：任意 Origin/方法/头。Python 为锚定白名单正则。叠加 N1 → 任意恶意网页可跨域调用全部本地 API | app.rs L431 vs app.py L32-42/L224-230 |
| N3 | **WS 鉴权退化 + token 泄漏**：只取第 2 个 subprotocol、非恒定时间比较、`eprintln!("WS_AUTH: config_token='{}'")` 明文打印 token 到 stderr | ws.rs L540-553 vs app.py L198-206 |
| N4 | **Origin 白名单可绕过**：`starts_with` 前缀匹配，`http://localhost.evil.com` 可通过；缺 https://127.0.0.1 | ws.rs L259-263 |
| N5 | **/ws/events 无鉴权无 Origin 检查** | events_ws.rs 全文 vs app.py L1913-1932 |

### 5.2 权限与引擎类（新 P0）

| # | 问题 | 证据 |
| --- | --- | --- |
| N6 | **写路径 scope 根传错**：WS 两处把 `data_dir/permissions.json`（文件路径）当 workspace_root 传入 → 任何指向真实工作区的写调用被硬拒（不弹审批） | ws.rs L343-345、L921-923 |
| N7 | **会话 mode 不生效**：PermissionEngine::new 恒 Interactive，无任何 set_mode 调用到会话引擎；WS set_mode 只改 SessionMeta | ws.rs L786-831；permissions.rs |
| N8 | **plan 批准不翻转 mode**：Python 批准后切换到结果指定模式继续执行；Rust 从不 set_mode → Plan 模式下批准后写操作全被 read-only 门拒绝 | engine.rs L921-969 vs engine.py L706-771 |
| N9 | **allowlist/standing rules 零注入**：set_allowed_commands、set_task_rules 全 crates 零调用；trust 合并出的 allowed_commands 不进判定 | permissions.rs |
| N10 | **extract_target 字面量 bug**：`"mcp__*"` 是精确匹配非 glob → 真实 MCP 工具名永不命中规则 | permissions.rs L381-393 |
| N11 | **risk 默认分类相反**：Rust 未知工具→EXTERNAL（弹审批），Python→READ；`run_shell` 不在 Exec 名单（名单为 shell/bash/zsh）→ 命令 allowlist 对其永不生效；shell_task_kill 声明 low 无条件放行 | permissions.rs L110-125 vs risk.py L39-53 |
| N12 | **retry 恒拒绝**：引擎从不追加 error notice（`Message::notice` 零调用）→ `_tail_is_retriable_error` 守卫永不成立 | engine.rs L271-281 |
| N13 | **token usage 从不持久化**：事件带 usage 但无 model 键；Message::assistant 硬编码 usage: None → GUI 刷新后用量恒 0 | types.rs L99-112；ws.rs L997-1013 |
| N14 | **JSONL 消息字段缩水**：user 无 ts/source；assistant 无 ts/usage/reasoning/tool_calls；tool 消息与 notice 从不持久化 → GUI 重放丢工具卡/用量/错误标记（静默降级） | ws.rs L895-1013 vs engine.py L988-1035 |
| N15 | **并行工具中断留孤儿**：并行路径 cancel 时直接 break，剩余 handle 不记录结果 → 历史遗留无结果的 tool_call | engine.rs L760-772 |
| N16 | **无 mid-turn checkpoint**：Python 5 个 checkpoint 落盘点；Rust 仅 turn 结束 persist_turn | ws.rs vs app.py L1705-1711 |
| N17 | **引擎重建不载入历史**：重启/重连后引擎上下文仅 system messages | ws.rs L926-934 vs manager.py L362-422 |

### 5.3 WS/契约类（新 P1）

| # | 问题 | 证据 |
| --- | --- | --- |
| N18 | **WS 限流 bug**：`Instant::now().elapsed()` 恒 ≈0，窗口永不清除 → 每连接累计 30 条后永久拒绝 | ws.rs L601 |
| N19 | **question/directory/plan 不经 inbox**：仅进程内等待，断线/重启即丢失（Python 三者均 park 进 inbox） | ws.rs vs manager.py L717-825 |
| N20 | **多视图被顶掉**：ws_sessions 为 HashMap<session_id, Sender>，第二个 socket 覆盖第一个；Python 为 callback set | state.rs L1845-1855 |
| N21 | **nav-layout 参数名不兼容**：Rust 读 `layout`，GUI 按 Python 契约发 `nav_layout` | settings.rs L259-276 |
| N22 | set_mode 无枚举校验无 mid-turn 保护；set_model 不广播不同步多视图；user_message 长度按字节（中文限额缩水约 3 倍） | ws.rs L786-857 |

### 5.4 Provider 类（新 P0/P1）

| # | 问题 | 证据 |
| --- | --- | --- |
| N23 | **Anthropic 结构性错误**：system 构造成 `{"role":"system",...}` 对象（API 只收字符串/块数组）→ system 缓存断点失效且形状非法；连续同角色折叠分支死代码（`as_array_mut()` 对 Object 恒 None）→ 连续 user/tool_result 触发 API 400；图片块用 `mimeType`（应为 `media_type`） | anthropic.rs L344-347、L151-165、L230-241 |
| N24 | **Gemini 工具循环断裂**：tool 结果拼手写 JSON 当单条 user text；convert_messages 第二返回值在 build_body 被丢弃；schema 转换丢 required/description/enum；参数白名单 camelCase 与引擎 snake_case 不匹配 | gemini.rs L60-94、L212 |
| N25 | **Bedrock 认证错误**：无 SigV4，IAM access_key 直接当 Bearer；无 claude family 原生分支（全走 Converse）；无 stream | bedrock.rs L304-319 |
| N26 | **Vertex 多处断裂**：读 `"region"` 而 Python 存 `"location"`；role=tool 直接跳过（"simplify"）；API key 滥用于全 family；无 stream；Claude 路径按纯 JSON 解析 SSE | vertex.rs |
| N27 | **verify 为 stub**：无条件 ok:true，不做任何凭据验证 | settings.rs L287-292 |
| N28 | **凭据 env 回退短路**：get_or_build 恒传 `resolve_api_key(None, None, profile)` → OPENAI_API_KEY/ANTHROPIC_API_KEY 等环境变量完全失效；无键时静默 "placeholder" | router.rs L116/124/137 |
| N29 | OpenAI 缺三套自愈逻辑（_pin_reasoning_effort / _param_fix_retry / _maybe_salvage_tool_calls）；sidecar（_anthropic/_gemini extras）无持久化字段 → thinking 上下文跨轮断链 | openai.rs、types.rs |

### 5.5 数据/工具类（新 P1/P2）

| # | 问题 | 证据 |
| --- | --- | --- |
| N30 | **Memory 三连锁断**：服务端用内存版 Vec store（SQLiteMemoryStore 零引用，重启即丢）；REST 添加 workspace=None 而注入按 workspace 过滤 → REST 记忆永不进 prompt；无 remember/memory_update/memory_forget 工具 | state.rs L1262；app.rs L529-538；state.rs L1391-1402 |
| N31 | **load_skill 未注册**：LoadSkillTool 已实现但会话注册表零引用；catalog 注入的 prompt 却指示模型调用 load_skill → 指向不存在的工具 | skills/src/skill.rs L199-258；agents.rs |
| N32 | **缺失工具清单**：send_message/send_file（senders 纯函数就绪但无工具接线）、attribution、memory×3、scheduling×4、selfwake×2（sleep_for/sleep_until）、subscription 工具、replace_in_file/apply_patch/apply_unified_diff | crates/tools/src/lib.rs register_all vs agent.py L161-285 |
| N33 | **Personas 运行时不生效**：get_agent 硬编码 4 个 agent，未知 id 回退 Code；三方 persona 的 manifest system_prompt/tools/mcp/skills 不参与会话 | agents.rs L78-121 |
| N34 | **Automations 残留**：timezone 恒 UTC；schedule_human dow=parts[3]（month 位）bug；无 overlap guard；调度 run 恒记 success（run.error 从不赋值）；selfwake 整体缺失 | automations.rs L320/L371-374；scheduler.rs L148-172 |
| N35 | ~~environment_context（OS/git 快照）未注入 system prompt~~（已修复：environment.rs 移植 environment_context + Folder scope，并补齐 Narration/Memory 指引，见 state.rs build_system_messages）；web-search 恒 DDG 无 provider 切换 | state.rs L1357 |
| N36 | sessions 保存语义：Rust `INSERT OR REPLACE` vs Python `ON CONFLICT DO UPDATE`（保留 title）；无 canonicalize_workspaces | conversation.rs L251-265 |

---

## 6. 测试覆盖差距清单

Python 898 个 `def test_`（实际收集 948）vs Rust 116 个 `#[test]`，行为级覆盖约 5-10%。Rust 测试集中于数据结构序列化/解析辅助/store 层。

按风险排序的最大缺口：

| # | 主题域 | Python 测试数 | Rust 对应 | 风险 |
| --- | --- | --- | --- | --- |
| 1 | Provider 请求构造/路由/校验 | 182 | 2（仅 OpenAI 流式聚合） | 高 — §5.4 全部问题无测试防护 |
| 2 | Engine 循环/流式/停止/恢复 | 84 | 7（边缘行为） | 高 — N8/N12/N15/N16 无防护 |
| 3 | 权限/risk/常驻审批 | 46 | 1 | 高 — N6/N9/N10/N11 无防护 |
| 4 | Server HTTP/WS API | 57 | 0（无 handler 级测试） | 高 — N18/N19/N21 无防护 |
| 5 | Tools 执行层 | 60 | 0 | 中 |
| 6 | Slack relay/status/审批人 | 55 | ~12（纯函数级） | 中 |
| 7 | 工作区信任面（allowlist/multiroot/catalog） | 30 | 2 | 中 — 访问控制边界 |
| 8 | 自动化调度执行 | 21 | 4（store/cron） | 中 — 调度执行零测试 |

---

## 7. 修复建议与优先级（仅建议，不实施）

### P0-Sec（安全，最优先）
1. 补 HTTP 鉴权中间件（x-openworker-token，恒定时间比较，tokenless_paths 白名单），启动时无 env 自动生成 token（对照 run.py `_ensure_api_token`）。
2. CORS 收紧为 Python 同款锚定正则白名单；WS Origin 改正则匹配并补 https://127.0.1。
3. 删除 ws.rs L550-553 的 token 明文打印；WS subprotocol 遍历全部候选 + 恒定时间比较。
4. /ws/events 补同款鉴权。

### P0-Func（切流阻断）
5. 修 N6：PermissionEngine workspace_root 传会话真实工作区路径。
6. 修 N7/N8：会话 mode 应用到引擎；plan 批准后 set_mode 翻转。
7. 修 N9/N10/N11：allowlist/task_rules 注入管道、extract_target glob 化、risk 基表对齐 Python（run_shell→Exec、默认 READ）。
8. 修 provider 四家断裂：Anthropic system 形状/折叠/图片键名；Gemini function_response 回放；Bedrock SigV4+family 分发；Vertex location 字段+tool 回放；五家真流式（去掉 resp.text() 整读）。
9. 修 N28：凭据 env 回退接通；N27 verify 真实校验。
10. inbox durable resume + mid-turn checkpoint + 引擎重建载入历史（N16/N17/P0-6）。
11. Gateway 挂载与入站编排（P1-10）：allowlist 执行、reply-token、interaction、mention、mirror。
12. MCP tools/call 接入会话注册表（P0-4 真缺口）。

### P1（可感知缺口）
13. N18 限流时钟 bug；N19 三类 prompt 进 inbox；N20 多视图广播；N21 参数名；N13/N14 usage 与消息字段落盘；N12 error notice 与 retry。
14. Memory 三连锁（N30）；load_skill 注册（N31）；persona 运行时接线（N33）。
15. Automations：timezone、schedule_human 一行修复、overlap guard、scheduling agent 工具、run.error 赋值。
16. 缺失工具接线：send_message/send_file/attribution/memory/scheduling/selfwake/subscription。

### P2/P3
17. 错误映射友好文案补齐；~~environment_context 注入~~（已完成）；web-search provider 切换；conversation save 语义对齐。
18. 测试补齐按 §6 顺序：先 provider 请求构造与 engine 循环，再权限与 handler 级测试。

---

## 附录：核验执行记录

- Rust 基线：`cargo check --workspace` 通过；`cargo test --workspace` 116 passed / 0 failed（cargo 1.94.1）。
- Python 基线：uv venv Python 3.12.13 + `pip install -e ".[messaging,dev,bedrock]"`；`pytest tests -q` → 947 passed / 1 skipped / 0 failed（41.1s）。
- 路由对账：`scripts/rust_route_parity.py` → Python 122 / Rust 133 / 重合 121 / Python-only 1（`/v1/_debug/inject_inbound`）/ Rust-only 12（skills CRUD、sessions create/get、skills 会话开关、通用 connector status）。
- 源码对照覆盖：S1 引擎与会话核心、S2 Provider 层、S3 服务器层与 manager、S4 连接器与入站链路、S5 MCP/自动化/工具层及其余子系统。
