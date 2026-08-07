#!/usr/bin/env python3
"""Build script: compiles the Rust workspace and standalone binaries.

Usage:
    python scripts/build_ocw_data.py              # build full workspace (release)
    python scripts/build_ocw_data.py --debug       # debug build
    python scripts/build_ocw_data.py --server      # build ocw-server only
    python scripts/build_ocw_data.py --data        # build ocw-data PyO3 extension
    python scripts/build_ocw_data.py --check       # cargo check only (fast verify)

This produces binaries under crates/target/release/:
    ocw-server    – HTTP/WS server (standalone)
    ocw-cli       – CLI tool

When --data is requested (PyO3 extension for the Python sidecar):
    crates/data/target/release/libocw_data.dylib  (macOS)
    crates/data/target/release/libocw_data.so      (Linux)
    crates/data/target/release/ocw_data.pyd       (Windows)
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import platform
from pathlib import Path

ROOT = Path(__file__).parent.parent.resolve()
CRATES = ROOT / "crates"


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    label = " ".join(str(c) for c in cmd)
    print(f"  $ {label}")
    return subprocess.run(cmd, cwd=kwargs.pop("cwd", CRATES), **kwargs)


def check_rust() -> str:
    cargo = shutil.which("cargo")
    if not cargo:
        print("ERROR: cargo not found. Install Rust via https://rustup.rs")
        sys.exit(1)
    result = run(["cargo", "--version"], capture_output=True, text=True)
    ver = result.stdout.strip() if result.stdout else "unknown"
    print(f"  Rust toolchain: {ver}")
    return cargo


def build_workspace(release: bool) -> None:
    """Build the entire Cargo workspace."""
    mode = "--release" if release else ""
    flags = [mode] if mode else []
    result = run(["cargo", "build", "--workspace", *flags], capture_output=False)
    if result.returncode != 0:
        print("ERROR: workspace build failed")
        sys.exit(1)


def build_server(release: bool) -> None:
    """Build the ocw-server binary only."""
    mode = "--release" if release else ""
    flags = [mode] if mode else []
    result = run(["cargo", "build", "-p", "ocw-server", *flags], capture_output=False)
    if result.returncode != 0:
        print("ERROR: ocw-server build failed")
        sys.exit(1)

    profile = "release" if release else "debug"
    target_dir = CRATES / "target" / profile
    if platform.system() == "Windows":
        exe = target_dir / "ocw-server.exe"
    else:
        exe = target_dir / "ocw-server"
    if exe.exists():
        print(f"  -> {exe}")
    else:
        print(f"  WARNING: binary not found at {exe}")


def build_data_extension() -> None:
    """Build ocw-data as a PyO3 native extension module (for Python sidecar)."""
    print("[data] Checking PyO3 compilation...")
    result = run(
        ["cargo", "check", "-p", "ocw-data", "--features", "pyo3"],
        capture_output=True,
    )
    if result.returncode != 0:
        print(f"ERROR: cargo check failed:\n{result.stderr.decode()}")
        sys.exit(1)
    print("  OK (no PyO3 compile errors)")

    print("[data] Building release binary with pyo3 feature...")
    result = run(
        [
            "cargo", "build",
            "-p", "ocw-data",
            "--release",
            "--features", "pyo3",
        ],
        capture_output=True,
        env={
            **os.environ,
            "MACOSX_DEPLOYMENT_TARGET": os.environ.get(
                "MACOSX_DEPLOYMENT_TARGET", "12.0"
            ),
        },
    )
    if result.returncode != 0:
        print(f"ERROR: cargo build failed:\n{result.stderr.decode()}")
        sys.exit(1)

    artifact_dir = CRATES / "data" / "target" / "release"
    if platform.system() == "Darwin":
        candidates = list(artifact_dir.glob("libocw_data*.dylib"))
    elif platform.system() == "Windows":
        candidates = list(artifact_dir.glob("ocw_data*.pyd"))
    else:
        candidates = list(artifact_dir.glob("libocw_data*.so"))

    if not candidates:
        print(f"ERROR: No extension artifact found in {artifact_dir}")
        print("  Contents:", list(artifact_dir.glob("*")))
        sys.exit(1)

    lib = candidates[0]
    dest = (ROOT / "coworker" / "ocw_data").with_suffix(lib.suffix)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(lib, dest)
    print(f"[data] Copied {lib.name} -> {dest.relative_to(ROOT)}")
    print(f"  From Python:  import ocw_data")


def cargo_check() -> None:
    """Fast verification — cargo check only, no codegen."""
    result = run(
        ["cargo", "check", "--workspace", "--all-targets"],
        capture_output=False,
    )
    if result.returncode != 0:
        print("ERROR: cargo check failed")
        sys.exit(1)
    print("  OK")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Build the OpenWorker Rust workspace and standalone binaries."
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Build in debug mode (default: release).",
    )
    parser.add_argument(
        "--server",
        action="store_true",
        help="Build only the ocw-server binary.",
    )
    parser.add_argument(
        "--data",
        action="store_true",
        help="Build only the ocw-data PyO3 extension (for Python sidecar).",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="cargo check only (fast compile verification, no binary output).",
    )
    args = parser.parse_args()

    print("OpenWorker Rust build helper")
    check_rust()
    print()

    if args.check:
        print("[check] cargo check --workspace --all-targets")
        cargo_check()
    elif args.server:
        profile = "debug" if args.debug else "release"
        print(f"[server] build ocw-server ({profile})")
        build_server(release=not args.debug)
    elif args.data:
        print("[data] build ocw-data PyO3 extension")
        build_data_extension()
    else:
        profile = "debug" if args.debug else "release"
        print(f"[workspace] build --workspace ({profile})")
        build_workspace(release=not args.debug)

    print()
    print("SUCCESS")


if __name__ == "__main__":
    main()
#!/usr/bin/env python3
"""Build script: compiles the Rust data layer as a PyO3 extension module.

Usage:
    python scripts/build_ocw_data.py

This produces:
    crates/data/target/release/libocw_data.dylib  (macOS)
    crates/data/target/release/libocw_data.so      (Linux)
    crates/data/target/release/libocw_data.pyd     (Windows)

The extension is loaded from Python as:
    import ocw_data
    store = ocw_data.ConversationStore("/path/to/config")
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import platform
from pathlib import Path

ROOT = Path(__file__).parent.parent.resolve()
CRATES = ROOT / "crates"


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    print(f"  $ {' '.join(str(c) for c in cmd)}")
    return subprocess.run(cmd, cwd=CRATES, **kwargs)


def main() -> None:
    cargo = shutil.which("cargo")
    if not cargo:
        print("ERROR: cargo not found. Install Rust via https://rustup.rs")
        sys.exit(1)

    # Ensure PyO3 target is configured for extension module
    print("[1/3] Checking Rust toolchain...")
    result = run(["cargo", "check", "-p", "ocw-data", "--features", "pyo3"], capture_output=True)
    if result.returncode != 0:
        print(f"ERROR: cargo check failed:\n{result.stderr.decode()}")
        sys.exit(1)
    print("  OK (no PyO3 compile errors)")

    print("[2/3] Building release binary with pyo3 feature...")
    # Build as cdylib extension module
    result = run(
        [
            "cargo", "build",
            "-p", "ocw-data",
            "--release",
            "--features", "pyo3",
            "--",
            # Tell PyO3 to build a native extension
            "-C", "link-args=-undefined dynamic_lookup",
        ],
        capture_output=True,
        env={**os.environ, "MACOSX_DEPLOYMENT_TARGET": "12.0"},
    )
    if result.returncode != 0:
        print(f"ERROR: cargo build failed:\n{result.stderr.decode()}")
        sys.exit(1)

    # Find the built library
    artifact_dir = CRATES / "data" / "target" / "release"
    if platform.system() == "Darwin":
        candidates = list(artifact_dir.glob("libocw_data*.dylib"))
    elif platform.system() == "Windows":
        candidates = list(artifact_dir.glob("ocw_data*.pyd"))
    else:
        candidates = list(artifact_dir.glob("libocw_data*.so"))

    if not candidates:
        print(f"ERROR: No extension artifact found in {artifact_dir}")
        print("  Contents:", list(artifact_dir.glob("*")))
        sys.exit(1)

    lib = candidates[0]
    dest = (ROOT / "coworker" / "ocw_data").with_suffix(lib.suffix)
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(lib, dest)
    print(f"[3/3] Copied {lib.name} → {dest.relative_to(ROOT)}")
    print()
    print(f"SUCCESS: {dest.name} built and placed in coworker/.")
    print("  From Python:  import ocw_data")
    print("  Usage:  store = ocw_data.ConversationStore('/path/to/config')")


if __name__ == "__main__":
    main()
