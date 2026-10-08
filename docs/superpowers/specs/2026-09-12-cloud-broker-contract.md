# Cloud Broker HTTP/WS Contract

**Status:** desktop-client freeze (2026-09-12). Self-hosted broker is a follow-on
service; this document is the acceptance contract derived from
[`coworker/cloud.py`](../../../coworker/cloud.py) and
[`crates/server/src/cloud.rs`](../../../crates/server/src/cloud.rs).

Desktop never embeds OAuth client secrets. Connector **access tokens** live only in
the local SecretStore. The broker holds vendor OAuth apps (and the GitHub App
private key), exchanges codes, and form-POSTs tokens back to the desktop loopback.

Config knobs (desktop `config.toml` / env):

| Key | Role |
| --- | --- |
| `cloud_base_url` | Broker HTTPS origin (prod default `https://api.openworker.com`) |
| `cloud_auth_domain` | OIDC issuer host (prod Auth0 `opencoworker.us.auth0.com`) |
| `cloud_client_id` | Public OIDC client id |
| `cloud_audience` | OIDC API audience |
| `cloud_relay_ws_url` | Managed inbound relay WSS (empty disables relay) |

Switching to a self-hosted broker = point these five keys at your deployment.
Do **not** special-case `api.openworker.com` in desktop code.

---

## 1. User sign-in (OIDC + broker bounce)

Desktop builds PKCE and opens the IdP authorize URL with:

- `redirect_uri` = `{cloud_base_url}/v1/auth/callback` (stable public URL)
- `state` = `{random}.{sidecar_port}` so the broker can bounce to loopback

### `GET {cloud_base_url}/v1/auth/callback`

Broker receives the IdP redirect, then **302/HTML bounce** to:

`http://127.0.0.1:{port}/auth/callback?code=…&state=…`

using the `.port` suffix of `state`.

Desktop then exchanges the code **directly against the IdP**
(`POST https://{cloud_auth_domain}/oauth/token`), not against the broker.
Self-host: keep an OIDC IdP (Auth0 / Zitadel / Keycloak) and only reimplement the bounce.

### `GET {cloud_base_url}/v1/me`

- Auth: `Authorization: Bearer {cloud_access_token}`
- 200 JSON: at least `{ "user": { "email": "…", "user_id": "…" } }`

---

## 2. Managed connector OAuth

### Provider map (desktop `PROVIDER_FOR_CONNECTOR`)

| Connector id | Broker `{provider}` |
| --- | --- |
| `gmail`, `google_calendar`, `google_drive` | `google` |
| `outlook` | `microsoft` |
| `slack` | `slack` |
| `notion` | `notion` |
| `attio` | `attio` |
| `hubspot` | `hubspot` |
| `github` | `github` |

### `POST {cloud_base_url}/v1/oauth/{provider}/start`

Auth: Bearer cloud access token.

Request JSON:

```json
{
  "connector": "gmail",
  "redirect": "http://127.0.0.1:{port}/oauth/callback",
  "app_state": "{desktop_random}",
  "access": "read|write",
  "flow": ""
}
```

- `access` — optional consent tier name (e.g. HubSpot); broker owns scopes.
- `flow` — GitHub only: `""` = App install; `"authorize"` = link to existing install.

Response 200:

```json
{ "authorize_url": "https://…" }
```

Desktop opens `authorize_url` in the system browser and remembers `app_state`
(~10 min TTL, single use).

### Vendor → broker → desktop form-POST

After the user consents, the broker **form-POSTs** to the `redirect` URL:

`POST http://127.0.0.1:{port}/oauth/callback`  
`Content-Type: application/x-www-form-urlencoded`

Common fields:

| Field | Notes |
| --- | --- |
| `connector` | Canonical connector id |
| `app_state` | Must match pending desktop state |
| `provider` | Broker provider key |
| `access_token` | Required except GitHub install path |
| `refresh_token` | Optional |
| `connection_id` | Broker connection metadata id |
| `account` | Display account (email / workspace name) |
| `account_id` | Stable id for multi-account connectors |
| `expires_in` | Seconds; omit for non-expiring tokens |
| `scope` | Optional |
| `error` | On failure |
| GitHub-only | `installation_id`, `account_login`, `account_type`, `repo_selection`, `github_login` — **no** access_token |
| Slack-only | `team_id`, bot token fields for relay mode |

Desktop `consume_managed_state(app_state)` must succeed exactly once; otherwise 400.

### `POST {cloud_base_url}/v1/oauth/{provider}/refresh`

Auth: Bearer cloud token.

```json
{
  "refresh_token": "…",
  "connection_id": "…",
  "connector": "gmail"
}
```

Response 200:

```json
{
  "access_token": "…",
  "refresh_token": "…",
  "expires_in": 3600
}
```

Desktop updates the local profile in place. Manual (non-managed) profiles are never refreshed.

### `POST {cloud_base_url}/v1/connections/{connection_id}/disconnect`

Auth: Bearer. Best-effort; desktop always deletes local secrets regardless.

### `GET {cloud_base_url}/v1/connections`

Auth: Bearer. Returns `{ "connections": [ … ] }`. On sign-in, desktop **only**
restores GitHub install routing metadata (`connector=github`, `status=connected`).
Other connectors keep tokens local-only and need re-consent.

---

## 3. GitHub App mint

### `POST {cloud_base_url}/v1/github/token`

```json
{ "installation_id": "101" }
```

Response 200: `{ "token": "ghs_…", "expires_at": "ISO-8601" }`.

Desktop caches the token **in memory only** (~50 min); never writes it to SecretStore.

### `POST {cloud_base_url}/v1/relay/github/disconnect`

```json
{ "installation_id": "101" }
```

Best-effort: stop pushing events for that install.

### `POST {cloud_base_url}/v1/relay/slack/uninstall`

```json
{ "team_id": "T…" }
```

Best-effort: stop pushing events for that workspace.

---

## 4. Managed inbound relay (WebSocket)

URL: `cloud_relay_ws_url` (prod is an API Gateway stage ending in `/ocw-connect`).

- Desktop opens WSS with `Authorization: Bearer {cloud_access_token}`.
- Broker pushes JSON frames tagged with `provider` (`slack` | `github`).
- Slack / GitHub relay adapters fan in on one shared hub.
- Replies still go desktop → vendor HTTP API with local bot / installation tokens
  (relay is inbound-only).

Exact frame schema should match Python `coworker/connectors/relay_client.py`
(`RelayHub` / `SlackRelayAdapter` / `GitHubRelayAdapter`). Self-host implementers
should freeze frames against those parsers + Rust `crates/connectors/src/relay.rs`.

---

## 5. Out of scope for the broker

- **MCP-backed connectors** (`jira`, `monday`, …): local OAuth 2.1 + PKCE + DCR on
  the desktop (`/mcp/oauth/callback`). Tokens in `mcp-oauth:{name}`. No broker.
- **Manual token paste**: always available signed out; never needs the broker.
- **Telemetry / persona gallery**: optional broker extras (`/v1/telemetry/events`,
  `/v1/personas/gallery…`); empty stubs are fine for a minimal self-host.

---

## 6. Suggested self-host slices

1. OIDC IdP + `/v1/auth/callback` bounce + `/v1/me`
2. `/v1/oauth/{provider}/start|refresh` + loopback form-POST for one provider (e.g. Google)
3. Expand providers; add GitHub App `/v1/github/token`
4. Relay WSS + Slack/GitHub event ingress

Register vendor OAuth apps with **redirect URIs pointing at your broker**, not at
desktop loopback ports.
