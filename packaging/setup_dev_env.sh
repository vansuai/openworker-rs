#!/usr/bin/env bash
# One-time dev bootstrap for a fresh checkout.
#
# Default path (Rust agent server):
#   - Ensures Rust toolchain can build ocw-server
#   - Optionally creates a Python venv for the Textual TUI / pytest / OCW_SIDECAR=python rollback
#
# Usage: bash packaging/setup_dev_env.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$ROOT/.venv"

echo "==> building ocw-server (dev)"
cargo build -p ocw-server --manifest-path "$ROOT/crates/Cargo.toml"
echo "    binary: $ROOT/crates/target/debug/ocw-server"

echo "==> Python venv (TUI / tests / optional Python sidecar)"
python3 -m venv "$VENV"
# The coworker package (server, engine, connectors) + inbound-messaging extras.
# aisuite comes in as a regular dependency (git-pinned in pyproject.toml until
# the next PyPI release).
"$VENV/bin/pip" install --quiet --upgrade pip
"$VENV/bin/pip" install --quiet -e "$ROOT[messaging,dev]"

"$VENV/bin/python" -c 'import aisuite, coworker' # fail loudly if the wiring broke
echo "Ready."
echo "  Rust server:  $ROOT/crates/target/debug/ocw-server --workspace /path/to/project --port 8765"
echo "  Python fallback: $VENV/bin/openworker-server --cwd /path/to/your/project --port 8765"
echo "  TUI:          $VENV/bin/openworker"
echo "  Desktop:      cd surfaces/gui && npm run tauri dev"
echo "  Pack rollback: OCW_SIDECAR=python ./packaging/build_dmg.sh"
