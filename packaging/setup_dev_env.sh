#!/usr/bin/env bash
# Dev bootstrap for a fresh checkout.
#
# Product path: Rust `ocw-server`.
# `coworker/` Python is migration reference only (pytest / optional TUI read).
#
# Usage: bash packaging/setup_dev_env.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$ROOT/.venv"

echo "==> building ocw-server (product runtime)"
cargo build -p ocw-server --manifest-path "$ROOT/crates/Cargo.toml"
echo "    binary: $ROOT/crates/target/debug/ocw-server"

echo "==> Python venv (reference tests / migration parity only)"
python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip
"$VENV/bin/pip" install --quiet -e "$ROOT[messaging,dev]"

"$VENV/bin/python" -c 'import aisuite, coworker' # fail loudly if the wiring broke
echo "Ready."
echo "  Product server: $ROOT/crates/target/debug/ocw-server --workspace /path/to/project --port 8765"
echo "  Desktop:        cd surfaces/gui && npm run tauri dev"
echo "  Reference tests: $VENV/bin/pytest"
echo "  Emergency pack: OCW_SIDECAR=python ./packaging/build_dmg.sh  # migration-only"
