#!/usr/bin/env python3
"""Static HTTP/WS route parity probe: Python FastAPI vs Rust axum.

Fails if GUI-critical Python-only routes are missing from crates/server/src/app.rs.
Rust-only supersets (skills CRUD, etc.) are allowed.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PY = ROOT / "coworker" / "server" / "app.py"
RS = ROOT / "crates" / "server" / "src" / "app.rs"

# Paths the GUI hard-codes that must exist on Rust (method, path template).
REQUIRED = {
    ("GET", "/v1/connectors/slack/status"),
    ("GET", "/v1/connectors/github/status"),
    ("POST", "/v1/connectors/github/installations/{installation_id}/disconnect"),
    ("GET", "/v1/agents"),
    ("GET", "/v1/health"),
    ("WS", "/ws/session/{session_id}"),
    ("WS", "/ws/events"),
}


def py_routes(text: str) -> set[tuple[str, str]]:
    out: set[tuple[str, str]] = set()
    for m in re.finditer(
        r'@app\.(get|post|patch|delete|put|websocket)\(\s*["\']([^"\']+)["\']',
        text,
    ):
        verb = m.group(1).upper()
        path = m.group(2)
        if verb == "WEBSOCKET":
            verb = "WS"
        out.add((verb, path))
    return out


def rs_routes(text: str) -> set[tuple[str, str]]:
    out: set[tuple[str, str]] = set()
    # Multiline: .route(\n  "/path",\n  get(...)) or axum::routing::get
    for m in re.finditer(
        r'\.route\(\s*"([^"]+)"\s*,\s*(?:axum::routing::)?(get|post|patch|delete|put)\s*\(',
        text,
        flags=re.MULTILINE | re.DOTALL,
    ):
        path, verb = m.group(1), m.group(2).upper()
        if path.startswith("/ws/"):
            out.add(("WS", path))
        else:
            out.add((verb, path))
    return out


def main() -> int:
    py = py_routes(PY.read_text())
    rs = rs_routes(RS.read_text())
    missing = sorted(REQUIRED - rs)
    if missing:
        print("FAIL: required routes missing from Rust app.rs:")
        for v, p in missing:
            print(f"  {v:6} {p}")
        return 1
    only_py = sorted(py - rs)
    only_rs = sorted(rs - py)
    print(f"Python routes: {len(py)}")
    print(f"Rust routes:   {len(rs)}")
    print(f"Overlap (approx): {len(py & rs)}")
    print(f"Python-only: {len(only_py)}")
    print(f"Rust-only:   {len(only_rs)}")
    print("Required GUI routes: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
