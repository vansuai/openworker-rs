# OpenWorker

**[openworker.com](https://openworker.com)** · [Download](#download) · [Issues](https://github.com/andrewyng/openworker/issues)

<a href="https://trendshift.io/repositories/91434?utm_source=trendshift-badge&amp;utm_medium=badge&amp;utm_campaign=badge-trendshift-91434" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/trendshift/repositories/91434/daily?language=Python" alt="andrewyng%2Fopenworker | Trendshift" width="250" height="55"/></a>

> **Beta** - OpenWorker is in open beta: fully usable, updates itself, and we're actively polishing rough edges. [Issues](https://github.com/andrewyng/openworker/issues) welcome.

**AI that gets your everyday tasks done.** OpenWorker is an open-source AI coworker that lives on your desktop and delivers **finished work**, not just chat: a polished document, a Slack reply with the numbers, an updated calendar, a triaged inbox.

It runs on your machine and doesn't lock you into any model: bring your own API key for OpenAI, Anthropic, Google, or an open-weight provider, or run fully local with Ollama. Your data leaves your machine only through the model and integrations *you* choose.

[![How OpenWorker works](docs/assets/how-it-works.png)](https://openworker.com)

## Download

[**⬇ macOS (Apple Silicon)**](https://download.openworker.com/mac)
<sub>macOS 12+ · signed & notarized · auto-updates</sub>

[**⬇ Windows 10/11 (x64)**](https://download.openworker.com/windows)
<sub>builds are not yet code-signed, so SmartScreen will warn; signing is in progress</sub>

Open the app, add a model key (or point it at Ollama), and ask for something real.

## How it works

1. Tell OpenWorker the outcome you want - "prepare a customer brief," "untangle my calendar," "draft a report," "check where the release stands across Jira and GitHub."
2. It breaks the task into steps and works across your desktop, files, and connected apps.
3. Before anything consequential - sending a message, changing a calendar, running a command - it checks in and you approve or redirect.
4. You get the finished deliverable, not a to-do list.

Under the hood:

```text
┌────────────────────────────────────────────────┐
│              OpenWorker desktop app            │  native shell + GUI
├────────────────────────────────────────────────┤
│           local agent server (Rust ocw-server) │  engine · tools · connectors
├───────────────┬────────────────┬───────────────┤
│  your files   │   your tools   │  your model   │  everything runs with your keys,
│  & terminal   │ 25+ connectors │  any provider │  on your machine
└───────────────┴────────────────┴───────────────┘
```

Migration note: `coworker/` (Python) is kept as a **read-only reference** for behavior/API parity while the Rust rewrite finishes. The shipped product path is `ocw-server`.

## What it can do

- **Produce real deliverables** - documents, spreadsheets, reports, and web pages land as files you can open and share.
- **Work from Slack** - mention `@OpenWorker` in a channel; a session opens on your desktop, the work happens with your tools, and the answer comes back as a thread reply.
- **Use your everyday tools** - 25+ integrations including GitHub, Slack, Jira, Notion, Linear, HubSpot, Outlook, monday.com, Gmail, and Google Calendar, plus your **terminal and local files**. Any tool reachable over [MCP](https://modelcontextprotocol.io/) plugs in too, with per-tool control.
- **Run on a schedule** - automations for recurring work: a morning brief, a weekly report, a standing watch over a channel. Runs land in the app with full transcripts.
- **Ask before acting** - writes, sends, and shell commands are approval-gated. Unattended runs park their asks in an inbox instead of acting on their own.

## Bring your own model

Model access is yours: pick a provider, paste your key, switch anytime. Supported out of the box:

**OpenAI · Anthropic · Google Gemini · Inkling (Thinking Machines) · GLM (Z.ai) · DeepSeek · Kimi (Moonshot) · Qwen · MiniMax · Mistral · Grok (xAI)** - plus open-weight models via **Together** and **Fireworks**, and fully local models via **Ollama**.

A curated model list marks what we've verified for tool-calling work. Adding any model string works at your own risk.

## Privacy

OpenWorker is local-first. Everything lives on your machine: the agent loop, your conversations, connector tokens, and model keys - all in the app's local secret store. The only cloud piece is a small service that brokers OAuth handshakes for connectors. You can always use the App without signing-in - use the connectors via manually-created credentials/API-keys.

## Development

### Prerequisites

- **Rust** toolchain via [rustup](https://rustup.rs/) (1.77+; the product server is Rust)
- **Node** 20+ and **pnpm** (or npm) for the GUI
- Python 3.10+ is only needed for the reference test suite (`tests/`), not the product server

### Quick start

```shell
git clone https://github.com/andrewyng/openworker
cd openworker

# 1. One-time bootstrap — builds ocw-server and sets up Python venv for tests
bash packaging/setup_dev_env.sh

# 2. Start the agent server standalone
./crates/target/debug/ocw-server --port 8765

# 3. In a second terminal, start the browser UI
cd surfaces/gui
pnpm install
pnpm dev           # → http://localhost:1420
```

The standalone server generates a per-launch auth token at `<state-dir>/sidecar-8765.token`
(`~/.config/coworker/` on macOS/Linux, `%APPDATA%\coworker\` on Windows). Vite reads
that file on startup. For direct API calls, pass the token in the `X-OpenWorker-Token` header.

### Tauri desktop app

To run the full desktop shell instead of the browser UI:

```shell
cd surfaces/gui
pnpm tauri dev
```

The Tauri shell automatically:
1. Builds `ocw-server` (debug profile).
2. Launches the server as a managed sidecar on a random free port.
3. Injects the HTTP/WS endpoints and an in-memory launch token into the WebView.
4. Starts Vite with HMR at `http://localhost:1420`.

The desktop app never writes the launch token to disk.

### Debugging

#### Frontend (React / Vite)

When running `pnpm tauri dev`, open Chrome DevTools in the Tauri window:
- **macOS**: `Cmd + Option + I` or right-click → Inspect
- **Windows**: `F12` or right-click → Inspect

Vite HMR is active — UI changes reflect instantly without restarting the app.

#### Rust backend (ocw-server)

Run the server standalone with logging for API-level debugging:

```shell
RUST_LOG=ocw_server=debug ./crates/target/debug/ocw-server --port 8765
```

When launched by the Tauri shell, server logs go to:

```shell
# Live tail
tail -f ~/.config/coworker/logs/openworker-server.log
```

Each launch overwrites the log; the previous run is kept as `.old`.

#### Rust debugger (LLDB)

```shell
cargo build -p ocw-server --manifest-path crates/Cargo.toml
rust-lldb crates/target/debug/ocw-server -- --port 8765
```

#### Troubleshooting sidecar startup

If the Tauri window gets stuck on "Starting coworker…":

1. Verify the server binary exists and is executable:
   ```shell
   ls -la crates/target/debug/ocw-server
   ```
2. Test the server standalone:
   ```shell
   crates/target/debug/ocw-server --port 18765
   ```
3. Check Tauri process logs on macOS:
   ```shell
   log stream --predicate 'processImagePath contains "openworker"' --level debug
   ```

### Common dev workflows

| Scenario | Command |
|---|---|
| Frontend-only changes | `cd surfaces/gui && pnpm tauri dev` — Vite HMR is instant after first launch |
| Rust server changes | `cargo check -p ocw-server --manifest-path crates/Cargo.toml`, then `pnpm tauri dev` |
| API debugging | `RUST_LOG=debug cargo run -p ocw-server --manifest-path crates/Cargo.toml` + curl |
| Full end-to-end | `pnpm tauri dev` in `surfaces/gui`, then `Cmd+Opt+I` for DevTools |

### Tests

```shell
# Rust server (no tests yet — migration in progress)
cargo test --manifest-path crates/Cargo.toml

# Python reference suite (behavior baseline)
.venv/bin/pytest

# GUI unit tests
cd surfaces/gui && pnpm test

# GUI end-to-end (hermetic)
cd surfaces/gui && pnpm e2e
```

Desktop bundles: `packaging/build_dmg.sh` (macOS) / `packaging/build_windows.ps1` (Windows).

## Repository layout

| Directory | What's in it |
|---|---|
| `crates/` | **Rust agent server** (`ocw-server`) — product runtime: engine, providers, tools, connectors, MCP |
| `coworker/` | Python reference implementation (migration baseline only; not the shipped server) |
| `surfaces/gui/` | React desktop UI + Tauri shell |
| `stt/` | Local speech-to-text (Whisper) used by the desktop shell |
| `packaging/` | Desktop installers; default sidecar is `ocw-server` |
| `docs/` | Design specs, parity audit, decision logs |
| `tests/` | Python reference test suite (behavior baseline for migration) |

## Built on aisuite

OpenWorker's engine is built on [**aisuite**](https://github.com/andrewyng/aisuite), a lightweight Python library providing a unified chat-completions API across LLM providers and an agents layer with tools, toolkits, and MCP support. If you want to build your own agent harness rather than use ours, start there; this repo is a working reference for what aisuite can carry.

OpenWorker was originally developed inside the aisuite repository before moving to its own home here; thanks to the aisuite contributors whose work it builds on.

## Contributing

Contributions and bug reports are welcome - open an [issue](https://github.com/andrewyng/openworker/issues) or a pull request. The app updates itself, so fixes reach installs quickly.
For any PR, please attach screenshots of what was broken and how it is fixed now. We will shortly add features that you can contribute to.
Please note that we are actively developing based off a internal list and goal, so we may not approve PRs that add features that are already under-development or deviates from our vision.

## License

MIT - see [LICENSE](LICENSE).
