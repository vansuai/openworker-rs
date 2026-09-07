#!/usr/bin/env bash
# Create public GitHub release repo, push main, set updater signing secrets.
#
# This network can reach api.github.com + git SSH, but github.com HTTPS may time out
# (so `gh auth login --web` device flow often fails). Prefer:
#
#   export GH_TOKEN=ghp_...   # classic PAT: repo + workflow
#   bash packaging/setup_github_updater.sh
#
# Or: bash packaging/setup_github_updater.sh ygqbasic/openworker-rs
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${1:-ygqbasic/openworker-rs}"
OWNER="${REPO%%/*}"
NAME="${REPO##*/}"
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

api() {
  local method="$1" path="$2"
  shift 2
  if [[ -n "${GH_TOKEN:-}" ]]; then
    curl -sS -X "$method" "https://api.github.com$path" \
      -H "Authorization: Bearer $GH_TOKEN" \
      -H "Accept: application/vnd.github+json" \
      -H "X-GitHub-Api-Version: 2022-11-28" \
      "$@"
  elif command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
    gh api -X "$method" "$path" "$@"
  else
    echo "error: set GH_TOKEN (PAT with repo+workflow) — github.com HTTPS device login is unreliable here" >&2
    exit 1
  fi
}

echo "==> ensuring public repo $REPO"
if ! api GET "/repos/$REPO" >/dev/null 2>&1; then
  api POST "/user/repos" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$NAME\",\"private\":false,\"description\":\"OpenWorker desktop — independent auto-update release source\",\"has_issues\":true,\"has_projects\":false,\"has_wiki\":false,\"auto_init\":false}" \
    >/dev/null
  echo "    created"
else
  echo "    already exists"
fi

if git remote get-url github >/dev/null 2>&1; then
  git remote set-url github "git@github.com:${REPO}.git"
else
  git remote add github "git@github.com:${REPO}.git"
fi

echo "==> pushing HEAD → github (main)"
git push -u github HEAD:main

echo "==> setting Actions secrets"
if command -v gh >/dev/null && { [[ -n "${GH_TOKEN:-}" ]] || gh auth status >/dev/null 2>&1; }; then
  export GH_TOKEN="${GH_TOKEN:-$(gh auth token 2>/dev/null || true)}"
  gh secret set TAURI_SIGNING_PRIVATE_KEY --repo "$REPO" <"$KEY_FILE"
  printf '%s' "$PASSWORD" | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo "$REPO"
else
  echo "error: gh CLI required to set secrets once GH_TOKEN is set" >&2
  exit 1
fi

echo "==> done"
echo "    endpoint: https://github.com/${REPO}/releases/latest/download/latest.json"
echo "    release:  bump tauri.conf.json version → git tag vX.Y.Z → git push github vX.Y.Z → Publish draft"
