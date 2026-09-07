#!/usr/bin/env bash
# Create public GitHub release repo, push main, set updater signing secrets.
#
# This network can reach api.github.com + git SSH, but github.com HTTPS may time out
# (so `gh auth login --web` device flow often fails). Prefer:
#
#   export GH_TOKEN=ghp_...   # classic PAT: repo + workflow
#   bash packaging/setup_github_updater.sh
#
# Or: bash packaging/setup_github_updater.sh vansuai/openworker-rs
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${1:-vansuai/openworker-rs}"
KEY_FILE="${TAURI_SIGNING_PRIVATE_KEY_PATH:-$HOME/.tauri/openworker-updater.key}"
ENV_FILE="${OCW_UPDATER_ENV:-$ROOT/../.ocw-updater.env}"

cd "$ROOT"

if [[ ! -f "$KEY_FILE" ]]; then
  echo "error: missing private key at $KEY_FILE" >&2
  exit 1
fi

PASSWORD=""
if [[ -f "$ENV_FILE" ]]; then
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
fi
: "${PASSWORD:?set TAURI_SIGNING_PRIVATE_KEY_PASSWORD or put it in $ENV_FILE}"

if ! command -v gh >/dev/null; then
  echo "error: gh CLI required (brew install gh)" >&2
  exit 1
fi
if [[ -z "${GH_TOKEN:-}" ]] && ! gh auth status >/dev/null 2>&1; then
  echo "error: set GH_TOKEN (PAT with repo+workflow) or run gh auth login" >&2
  exit 1
fi

echo "==> ensuring public repo $REPO"
if gh repo view "$REPO" >/dev/null 2>&1; then
  echo "    already exists"
else
  gh repo create "$REPO" --public \
    --description "OpenWorker desktop — independent auto-update release source"
  echo "    created"
fi

if git remote get-url github >/dev/null 2>&1; then
  git remote set-url github "git@github.com:${REPO}.git"
else
  git remote add github "git@github.com:${REPO}.git"
fi

echo "==> pushing HEAD → github (main)"
# Prefer HTTPS+token when SSH cannot see a brand-new repo yet; fall back to SSH.
if [[ -n "${GH_TOKEN:-}" ]]; then
  git push "https://x-access-token:${GH_TOKEN}@github.com/${REPO}.git" HEAD:main
  git fetch github main >/dev/null 2>&1 || true
  git branch --set-upstream-to="github/main" 2>/dev/null || true
else
  git push -u github HEAD:main
fi

echo "==> setting Actions secrets"
export GH_TOKEN="${GH_TOKEN:-$(gh auth token 2>/dev/null || true)}"
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo "$REPO" <"$KEY_FILE"
printf '%s' "$PASSWORD" | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo "$REPO"

echo "==> done"
echo "    endpoint: https://github.com/${REPO}/releases/latest/download/latest.json"
echo "    release:  bump tauri.conf.json version → git tag vX.Y.Z → git push github vX.Y.Z → Publish draft"
