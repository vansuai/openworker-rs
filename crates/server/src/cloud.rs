//! OpenWorker Cloud client: sign-in and managed one-click connectors.
//!
//! Everything here is OPTIONAL. The app is fully functional signed out —
//! manual token paste stays available for every connector. Cloud sign-in only
//! unlocks the one-click managed OAuth path and the metadata conveniences that
//! come with it. Ported from `coworker/cloud.py`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::state::{Config, SettingsManager};

pub const CLOUD_AUTH_PROFILE: &str = "cloud:auth";
pub const TELEMETRY_PROFILE: &str = "cloud:telemetry";
const LOGIN_SCOPES: &str = "openid profile email offline_access";

/// connector id (canonical, = descriptor name) -> broker provider key
const PROVIDER_FOR_CONNECTOR: &[(&str, &str)] = &[
    ("gmail", "google"),
    ("google_calendar", "google"),
    ("google_drive", "google"),
    ("slack", "slack"),
    ("notion", "notion"),
    ("attio", "attio"),
    ("hubspot", "hubspot"),
    ("github", "github"),
    ("outlook", "microsoft"),
];

fn provider_for(connector: &str) -> Option<&'static str> {
    PROVIDER_FOR_CONNECTOR
        .iter()
        .find(|(c, _)| *c == connector)
        .map(|(_, p)| *p)
}

// Pending PKCE verifiers keyed by OAuth state; in-process only. A login that
// outlives the sidecar process simply has to be restarted.
const PENDING_TTL: u64 = 600;
const MANAGED_STATE_TTL: u64 = 600;

struct PendingLogin {
    verifier: String,
    created: u64,
}

static PENDING_LOGINS: LazyLock<Mutex<HashMap<String, PendingLogin>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_MANAGED: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// GitHub installation tokens: memory-only by design (relay spec §4). They live
// ~1 h and are re-minted from the broker; nothing secret touches the store.
static GITHUB_TOKEN_CACHE: LazyLock<Mutex<HashMap<String, (String, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const GITHUB_TOKEN_LEEWAY: u64 = 600;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn token_urlsafe(n: usize) -> String {
    b64url(&random_bytes(n))
}

fn random_hex(n: usize) -> String {
    random_bytes(n.div_ceil(2))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(n)
        .collect()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

fn cloud_base_url(config: &Config) -> String {
    config
        .cloud_url
        .clone()
        .unwrap_or_else(|| "https://api.openworker.com".to_string())
}

fn cloud_auth_domain(config: &Config) -> String {
    config.cloud_auth_domain.clone()
}

fn cloud_client_id(config: &Config) -> String {
    config.cloud_client_id.clone()
}

fn cloud_audience(config: &Config) -> String {
    config.cloud_audience.clone()
}

fn get_str(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn auth_redirect_uri(config: &Config) -> String {
    format!("{}/v1/auth/callback", cloud_base_url(config).trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// sign-in (J1)
// ---------------------------------------------------------------------------

/// Create a PKCE login and return the browser URL. The sidecar's
/// `GET /auth/callback` completes it. The redirect goes through the broker's
/// stable callback, which bounces the browser to our actual loopback port
/// (carried as the state's `.port` suffix — Auth0 echoes state untouched).
pub fn begin_login(config: &Config) -> Value {
    let verifier = b64url(&random_bytes(48));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let port = std::env::var("COWORKER_PORT")
        .ok()
        .unwrap_or_else(|| config.port.to_string());
    let state = format!("{}.{}", token_urlsafe(16), port);

    {
        let mut pending = PENDING_LOGINS.lock().unwrap();
        let cutoff = now().saturating_sub(PENDING_TTL);
        pending.retain(|_, p| p.created >= cutoff);
        pending.insert(
            state.clone(),
            PendingLogin {
                verifier,
                created: now(),
            },
        );
    }

    let authorize_url = format!(
        "https://{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&audience={}&state={}&code_challenge={}&code_challenge_method=S256",
        cloud_auth_domain(config),
        urlencode(&cloud_client_id(config)),
        urlencode(&auth_redirect_uri(config)),
        urlencode(LOGIN_SCOPES),
        urlencode(&cloud_audience(config)),
        urlencode(&state),
        challenge,
    );
    json!({"authorize_url": authorize_url, "state": state})
}

/// Exchange the Auth0 code for tokens and store the session under `cloud:auth`.
/// Best-effort `/v1/me` fetch fills in the account identity for the GUI.
pub async fn complete_login(
    settings: &SettingsManager,
    config: &Config,
    code: &str,
    state: &str,
) -> Value {
    let pending = match PENDING_LOGINS.lock().unwrap().remove(state) {
        Some(p) if p.created >= now().saturating_sub(PENDING_TTL) => p,
        _ => return json!({"ok": false, "error": "unknown or expired sign-in attempt"}),
    };
    let resp = match client() {
        Ok(c) => c
            .post(format!("https://{}/oauth/token", cloud_auth_domain(config)))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", cloud_client_id(config).as_str()),
                ("code", code),
                ("code_verifier", pending.verifier.as_str()),
                ("redirect_uri", auth_redirect_uri(config).as_str()),
            ])
            .send(),
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let token: Value = match resp {
        Ok(r) if r.status().is_success() => r.json().unwrap_or(Value::Null),
        _ => return json!({"ok": false, "error": "token exchange failed"}),
    };
    store_cloud_tokens(settings, &token).await;

    // Best-effort profile fetch so the GUI can show who is signed in.
    if let Some(me) = fetch_me(settings, config).await {
        let mut profile = settings
            .secrets_get(CLOUD_AUTH_PROFILE)
            .await
            .unwrap_or_default();
        profile.insert(
            "account".into(),
            json!(me.pointer("/user/email")
                .and_then(|v| v.as_str())
                .unwrap_or("")),
        );
        profile.insert(
            "user_id".into(),
            json!(me.pointer("/user/user_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")),
        );
        settings.secrets_put(CLOUD_AUTH_PROFILE, profile).await;
    }
    let st = status(settings).await;
    let mut out = Map::new();
    out.insert("ok".into(), Value::Bool(true));
    if let Some(s) = st.as_object() {
        for (k, v) in s {
            out.insert(k.clone(), v.clone());
        }
    }
    json!(out)
}

async fn store_cloud_tokens(settings: &SettingsManager, token: &Value) {
    let mut profile = settings.secrets_get(CLOUD_AUTH_PROFILE).await.unwrap_or_else(|| {
        let mut m = Map::new();
        m.insert("type".into(), json!("oauth"));
        m.insert("enabled".into(), json!(true));
        m
    });
    profile.insert(
        "access_token".into(),
        token.get("access_token").cloned().unwrap_or(json!("")),
    );
    if token.get("refresh_token").is_some() {
        // rotating refresh tokens: keep the newest
        profile.insert("refresh_token".into(), token.get("refresh_token").cloned().unwrap());
    }
    let expires = now()
        + token
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600)
        - 60;
    profile.insert("expires".into(), json!(expires));
    settings.secrets_put(CLOUD_AUTH_PROFILE, profile).await;
}

pub async fn status(settings: &SettingsManager) -> Value {
    let profile = settings.secrets_get(CLOUD_AUTH_PROFILE).await.unwrap_or_default();
    json!({
        "signed_in": !get_str(&profile, "access_token").is_empty(),
        "account": get_str(&profile, "account"),
        "user_id": get_str(&profile, "user_id"),
    })
}

pub async fn logout(settings: &SettingsManager) -> Value {
    settings.secrets_delete(CLOUD_AUTH_PROFILE).await;
    json!({"ok": true, "signed_in": false})
}

/// Valid cloud session token, silently refreshed near expiry; None when signed
/// out or the session can't be renewed.
pub async fn fresh_access_token(
    settings: &SettingsManager,
    config: &Config,
) -> Option<String> {
    let profile = settings.secrets_get(CLOUD_AUTH_PROFILE).await.unwrap_or_default();
    let access = get_str(&profile, "access_token");
    if access.is_empty() {
        return None;
    }
    let expires = profile.get("expires").and_then(|v| v.as_u64()).unwrap_or(0);
    if expires > now() {
        return Some(access);
    }
    let refresh = get_str(&profile, "refresh_token");
    if refresh.is_empty() {
        return None;
    }
    let token: Value = client()
        .ok()?
        .post(format!("https://{}/oauth/token", cloud_auth_domain(config)))
        .form(&[
            ("grant_type", "refresh_token".to_string()),
            ("client_id", cloud_client_id(config)),
            ("refresh_token", refresh),
        ])
        .send()
        .ok()?
        .json()
        .ok()?;
    token.get("access_token")?;
    store_cloud_tokens(settings, &token).await;
    Some(
        settings
            .secrets_get(CLOUD_AUTH_PROFILE)
            .await
            .map(|m| get_str(&m, "access_token"))
            .unwrap_or_default(),
    )
}

async fn fetch_me(settings: &SettingsManager, config: &Config) -> Option<Value> {
    let token = fresh_access_token(settings, config).await?;
    let resp = client()
        .ok()?
        .get(format!("{}/v1/me", cloud_base_url(config).trim_end_matches('/')))
        .bearer_auth(&token)
        .send()
        .ok()?;
    if resp.status().is_success() {
        resp.json().ok()
    } else {
        None
    }
}

/// Rebuild local managed-connection state from the broker's metadata rows
/// after a cloud sign-in. Only GitHub restores fully on a fresh install (its
/// rows are routing metadata; tokens mint on demand).
pub async fn sync_connections(settings: &SettingsManager, config: &Config) -> Value {
    let Some(token) = fresh_access_token(settings, config).await else {
        return json!({"ok": false, "error": "not signed in"});
    };
    let resp = match client() {
        Ok(c) => c
            .get(format!(
                "{}/v1/connections",
                cloud_base_url(config).trim_end_matches('/')
            ))
            .bearer_auth(&token)
            .send(),
        Err(_) => return json!({"ok": false, "error": "cloud unreachable"}),
    };
    let data: Value = match resp {
        Ok(r) if r.status().is_success() => r.json().unwrap_or(Value::Null),
        Ok(r) => {
            return json!({"ok": false, "error": format!("connections fetch failed ({})", r.status())})
        }
        Err(_) => return json!({"ok": false, "error": "cloud unreachable"}),
    };
    let mut restored: Vec<String> = Vec::new();
    for row in data
        .get("connections")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if row.get("connector").and_then(|v| v.as_str()) != Some("github")
            || row.get("status").and_then(|v| v.as_str()) != Some("connected")
        {
            continue;
        }
        let meta = row.get("tenant_metadata").cloned().unwrap_or(json!({}));
        let mut installs: Vec<Value> = meta
            .get("installations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if installs.is_empty() && meta.get("installation_id").is_some() {
            installs.push(meta.clone()); // pre-restore-era rows carry the primary install
        }
        for inst in installs {
            let mut form = Map::new();
            for (k, v) in [
                ("installation_id", inst.get("installation_id")),
                ("account_login", inst.get("account_login")),
                ("account_type", inst.get("account_type")),
                ("repo_selection", inst.get("repo_selection")),
            ] {
                if let Some(v) = v {
                    form.insert(k.into(), v.clone());
                }
            }
            if let Some(g) = meta.get("github_login") {
                form.insert("github_login".into(), g.clone());
            }
            if let Some(c) = row.get("connection_id") {
                form.insert("connection_id".into(), c.clone());
            }
            let out = managed_connect_install(settings, &form).await;
            if out.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(id) = out.get("installation_id").and_then(|v| v.as_str()) {
                    restored.push(id.to_string());
                }
            }
        }
    }
    json!({"ok": true, "restored": restored})
}

// ---------------------------------------------------------------------------
// telemetry (J4)
// ---------------------------------------------------------------------------

/// Stable random per-install id, minted on first use.
pub async fn install_id(settings: &SettingsManager) -> String {
    let mut profile = settings
        .secrets_get(TELEMETRY_PROFILE)
        .await
        .unwrap_or_default();
    if let Some(id) = profile.get("install_id").and_then(|v| v.as_str()) {
        return id.to_string();
    }
    let id = format!("ins_{}", random_hex(12));
    profile.insert("install_id".into(), json!(id.clone()));
    settings.secrets_put(TELEMETRY_PROFILE, profile).await;
    id
}

pub async fn telemetry_enabled(settings: &SettingsManager) -> bool {
    settings
        .secrets_get(TELEMETRY_PROFILE)
        .await
        .map(|m| {
            m.get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        })
        .unwrap_or(true) // default-on (only matters signed in)
}

pub async fn set_telemetry_enabled(settings: &SettingsManager, enabled: bool) -> Value {
    let mut profile = settings
        .secrets_get(TELEMETRY_PROFILE)
        .await
        .unwrap_or_default();
    profile.insert("enabled".into(), json!(enabled));
    settings.secrets_put(TELEMETRY_PROFILE, profile).await;
    json!({"ok": true, "telemetry_enabled": enabled})
}

// ---------------------------------------------------------------------------
// managed connectors (J2)
// ---------------------------------------------------------------------------

/// Authenticated start: returns the provider consent URL for the browser.
/// `access` names a broker-defined consent tier; `flow` is GitHub-only.
pub async fn begin_managed_connect(
    settings: &SettingsManager,
    config: &Config,
    connector: &str,
    access: &str,
    flow: &str,
) -> Value {
    let Some(provider) = provider_for(connector) else {
        return json!({"ok": false, "error": format!("{connector} has no managed OAuth path")});
    };
    let Some(token) = fresh_access_token(settings, config).await else {
        return json!({"ok": false, "error": "not signed in", "signed_in": false});
    };
    let app_state = token_urlsafe(16);
    let port = std::env::var("COWORKER_PORT")
        .ok()
        .unwrap_or_else(|| config.port.to_string());
    let mut body = Map::new();
    body.insert("connector".into(), json!(connector));
    body.insert(
        "redirect".into(),
        json!(format!("http://127.0.0.1:{port}/oauth/callback")),
    );
    body.insert("app_state".into(), json!(app_state.clone()));
    if !access.is_empty() {
        body.insert("access".into(), json!(access));
    }
    if !flow.is_empty() {
        body.insert("flow".into(), json!(flow));
    }
    let resp = match client() {
        Ok(c) => c
            .post(format!(
                "{}/v1/oauth/{provider}/start",
                cloud_base_url(config).trim_end_matches('/')
            ))
            .bearer_auth(&token)
            .json(&body)
            .send(),
        Err(e) => return json!({"ok": false, "error": format!("cloud unreachable: {e}")}),
    };
    let data: Value = match resp {
        Ok(r) if r.status().is_success() => r.json().unwrap_or(Value::Null),
        Ok(r) => {
            return json!({"ok": false, "error": format!("start failed ({})", r.status())})
        }
        Err(e) => return json!({"ok": false, "error": format!("cloud unreachable: {e}")}),
    };
    PENDING_MANAGED
        .lock()
        .unwrap()
        .insert(app_state.clone(), now());
    json!({
        "ok": true,
        "authorize_url": data.get("authorize_url").cloned().unwrap_or(json!("")),
        "app_state": app_state,
    })
}

/// Consume one recent managed-OAuth callback state exactly once.
pub fn consume_managed_state(state: &str) -> bool {
    if state.is_empty() {
        return false;
    }
    let mut pending = PENDING_MANAGED.lock().unwrap();
    match pending.remove(state) {
        Some(created) => created >= now().saturating_sub(MANAGED_STATE_TTL),
        None => false,
    }
}

/// Local connector profile from the broker's form-POST payload. Field-
/// compatible with a manual paste so tools and gating treat both paths
/// identically; the managed extras enable broker refresh and cloud disconnect.
pub fn managed_profile_from_callback(form: &Map<String, Value>) -> Map<String, Value> {
    let mut profile = Map::new();
    profile.insert("type".into(), json!("oauth"));
    profile.insert("enabled".into(), json!(true));
    profile.insert("managed".into(), json!(true));
    for k in [
        "access_token",
        "refresh_token",
        "scope",
        "connection_id",
        "provider",
        "account",
    ] {
        profile.insert(k.into(), json!(get_str(form, k)));
    }
    let account_id = get_str(form, "account_id");
    if !account_id.is_empty() {
        // The stable id behind the display name — what the generic accounts
        // layer keys multi-account profiles by.
        profile.insert("account_id".into(), json!(account_id));
    }
    if let Some(expires_in) = form.get("expires_in").and_then(|v| v.as_u64()) {
        // absent ⇒ non-expiring token (e.g. Slack bot tokens)
        profile.insert("expires".into(), json!(now() + expires_in - 60));
    }
    profile
}

/// Store a managed GitHub App install from the broker's form-POST: writes
/// `github:install:<id>` (metadata only — the callback carries no token) and
/// flips `github:default` to relay mode. A manual PAT stays untouched.
pub async fn managed_connect_install(
    settings: &SettingsManager,
    form: &Map<String, Value>,
) -> Value {
    let installation_id = get_str(form, "installation_id")
        .trim()
        .to_string();
    if installation_id.is_empty() {
        return json!({"ok": false, "error": "installation_id missing from callback"});
    }
    let key = format!("github:install:{installation_id}");
    let existing = settings.secrets_get(&key).await.unwrap_or_default();
    let mut profile = Map::new();
    profile.insert("type".into(), json!("oauth"));
    profile.insert("managed".into(), json!(true));
    profile.insert("installation_id".into(), json!(installation_id.clone()));
    for k in [
        "account_login",
        "account_type",
        "github_login",
        "repo_selection",
        "connection_id",
    ] {
        profile.insert(k.into(), json!(get_str(form, k)));
    }
    if let Some(allowed) = existing.get("allowed_users").cloned() {
        profile.insert("allowed_users".into(), allowed);
    }
    if existing.get("allow_all").is_some() {
        profile.insert("allow_all".into(), json!(true));
    }
    settings.secrets_put(&key, profile).await;
    let mut default = settings.secrets_get("github:default").await.unwrap_or_default();
    default.insert("type".into(), json!("oauth"));
    default.insert("managed".into(), json!(true));
    default.insert("mode".into(), json!("relay"));
    default.insert("enabled".into(), json!(true));
    settings.secrets_put("github:default", default).await;
    json!({"ok": true, "installation_id": installation_id})
}

/// Store a managed Slack install (relay mode): `slack:team:<team_id>` holds the
/// workspace's bot token; `slack:default` flips to `mode="relay"`. Existing
/// allow-list preserved; the installer is pre-added (first mention consent).
pub async fn managed_connect_slack_install(
    settings: &SettingsManager,
    form: &Map<String, Value>,
) -> Value {
    let team_id = get_str(form, "team_id");
    let bot_token = get_str(form, "access_token");
    if team_id.is_empty() || bot_token.is_empty() {
        return json!({"ok": false, "error": "missing team_id or bot token"});
    }
    let key = format!("slack:team:{team_id}");
    let existing = settings.secrets_get(&key).await.unwrap_or_default();
    let mut allowed: Vec<String> = existing
        .get("allowed_users")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let installer = get_str(form, "slack_user_id");
    if !installer.is_empty() && !allowed.contains(&installer) {
        allowed.push(installer);
    }
    allowed.sort();
    let mut profile = Map::new();
    profile.insert("type".into(), json!("oauth"));
    profile.insert("managed".into(), json!(true));
    profile.insert("bot_token".into(), json!(bot_token));
    profile.insert("bot_user_id".into(), json!(get_str(form, "bot_user_id")));
    profile.insert("slack_user_id".into(), json!(get_str(form, "slack_user_id")));
    profile.insert("team_id".into(), json!(team_id));
    profile.insert("account".into(), json!(get_str(form, "account")));
    profile.insert("domain".into(), json!(get_str(form, "team_domain")));
    profile.insert("scope".into(), json!(get_str(form, "scope")));
    profile.insert("connection_id".into(), json!(get_str(form, "connection_id")));
    profile.insert("allowed_users".into(), json!(allowed));
    if existing.get("allow_all").is_some() {
        profile.insert("allow_all".into(), json!(true));
    }
    if let Some(sn) = existing.get("sender_name").cloned() {
        profile.insert("sender_name".into(), sn);
    }
    settings.secrets_put(&key, profile).await;
    let mut default = settings.secrets_get("slack:default").await.unwrap_or_default();
    default.insert("type".into(), json!("oauth"));
    default.insert("managed".into(), json!(true));
    default.insert("mode".into(), json!("relay"));
    default.insert("enabled".into(), json!(true));
    settings.secrets_put("slack:default", default).await;
    json!({"ok": true, "account": get_str(form, "account")})
}

/// Store one managed-OAuth account (gmail/gcal/hubspot layouts) under
/// `<pre><id>`; the first connected account becomes the default. `id_key` is
/// the form field holding the stable id ("account" for google mail/calendar,
/// "hub_id" for hubspot).
pub async fn managed_connect_account(
    settings: &SettingsManager,
    pre: &str,
    def_key: &str,
    pointer_field: &str,
    id_key: &str,
    profile: Map<String, Value>,
) -> Value {
    let account_id = profile
        .get(id_key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    if account_id.is_empty() {
        return json!({"ok": false, "error": format!("{id_key} missing from callback")});
    }
    settings
        .secrets_put(&format!("{pre}{account_id}"), profile)
        .await;
    let mut pointer = settings.secrets_get(def_key).await.unwrap_or_default();
    pointer.entry(pointer_field).or_insert(json!(account_id.clone()));
    pointer.insert("type".into(), json!("oauth"));
    pointer.insert("enabled".into(), json!(true));
    settings.secrets_put(def_key, pointer).await;
    json!({"ok": true, "account": account_id})
}

/// Renew a managed connector token through the broker. Returns the updated
/// profile, or None if this profile can't be (or doesn't need to be) renewed.
/// Manual profiles are never touched.
pub async fn refresh_managed_token(
    settings: &SettingsManager,
    config: &Config,
    connector: &str,
    profile_key: Option<&str>,
) -> Option<Value> {
    let key = profile_key
        .unwrap_or(&format!("{connector}:default"))
        .to_string();
    let profile = settings.secrets_get(&key).await.unwrap_or_default();
    let managed = profile.get("managed").and_then(|v| v.as_bool()).unwrap_or(false);
    let refresh = get_str(&profile, "refresh_token");
    if !managed || refresh.is_empty() {
        return None;
    }
    let provider = {
        let p = get_str(&profile, "provider");
        if p.is_empty() {
            provider_for(connector)?.to_string()
        } else {
            p
        }
    };
    let token = fresh_access_token(settings, config).await?;
    let body = json!({
        "refresh_token": refresh,
        "connection_id": get_str(&profile, "connection_id"),
        "connector": connector,
    });
    let resp = client()
        .ok()?
        .post(format!(
            "{}/v1/oauth/{provider}/refresh",
            cloud_base_url(config).trim_end_matches('/')
        ))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let fresh: Value = resp.json().ok()?;
    let mut profile = profile;
    profile.insert(
        "access_token".into(),
        fresh.get("access_token").cloned().unwrap_or(json!("")),
    );
    if fresh.get("refresh_token").is_some() {
        profile.insert("refresh_token".into(), fresh.get("refresh_token").cloned().unwrap());
    }
    let expires = now()
        + fresh
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600)
        - 60;
    profile.insert("expires".into(), json!(expires));
    settings.secrets_put(&key, profile.clone()).await;
    Some(Value::Object(profile))
}

/// Refresh-on-expiry hook for connector tools: if this is a managed profile
/// about to expire, renew it in place. No-op for manual profiles.
pub async fn ensure_fresh_connector_token(
    settings: &SettingsManager,
    config: &Config,
    connector: &str,
    profile_key: Option<&str>,
    leeway: u64,
) {
    let key = profile_key
        .unwrap_or(&format!("{connector}:default"))
        .to_string();
    let profile = settings.secrets_get(&key).await.unwrap_or_default();
    if !profile.get("managed").and_then(|v| v.as_bool()).unwrap_or(false) {
        return;
    }
    let expires = profile.get("expires").and_then(|v| v.as_u64()).unwrap_or(0);
    if expires > 0 && expires > now() + leeway {
        return;
    }
    refresh_managed_token(settings, config, connector, Some(&key)).await;
}

/// Best-effort: tell the cloud a managed connection is gone so its metadata
/// flips to disconnected. Local deletion always proceeds regardless.
pub async fn cloud_disconnect(
    settings: &SettingsManager,
    config: &Config,
    connector: &str,
    profile_key: Option<&str>,
) {
    let key = profile_key
        .unwrap_or(&format!("{connector}:default"))
        .to_string();
    let profile = settings.secrets_get(&key).await.unwrap_or_default();
    let managed = profile.get("managed").and_then(|v| v.as_bool()).unwrap_or(false);
    let connection_id = get_str(&profile, "connection_id");
    if !managed || connection_id.is_empty() {
        return;
    }
    let Some(token) = fresh_access_token(settings, config).await else {
        return;
    };
    if let Ok(c) = client() {
        let _ = c
            .post(format!(
                "{}/v1/connections/{}/disconnect",
                cloud_base_url(config).trim_end_matches('/'),
                connection_id
            ))
            .bearer_auth(&token)
            .send();
    }
}

/// A live installation access token for GitHub API calls, minted via the
/// authenticated broker route and cached in memory (~50 min). `force` skips
/// the cache — the 401 retry path. Empty string when unavailable.
pub async fn github_installation_token(
    settings: &SettingsManager,
    config: &Config,
    installation_id: &str,
    force: bool,
) -> String {
    let installation_id = installation_id.trim().to_string();
    if installation_id.is_empty() {
        return String::new();
    }
    if !force {
        let cache = GITHUB_TOKEN_CACHE.lock().unwrap();
        if let Some((token, expires)) = cache.get(&installation_id) {
            if *expires > now() + GITHUB_TOKEN_LEEWAY {
                return token.clone();
            }
        }
    }
    let Some(token) = fresh_access_token(settings, config).await else {
        return String::new();
    };
    let body = json!({"installation_id": installation_id.clone()});
    let Ok(c) = client() else {
        return String::new();
    };
    let Ok(resp) = c
        .post(format!(
            "{}/v1/github/token",
            cloud_base_url(config).trim_end_matches('/')
        ))
        .bearer_auth(&token)
        .json(&body)
        .send()
    else {
        return String::new();
    };
    if !resp.status().is_success() {
        return String::new();
    }
    let Ok(data) = resp.json::<Value>() else {
        return String::new();
    };
    let minted = data
        .as_object()
        .map(|m| get_str(m, "token"))
        .unwrap_or_default();
    // expires_at is ISO-8601 from GitHub; parse defensively, default 1 h.
    let expires = now() + 3600;
    if !minted.is_empty() {
        GITHUB_TOKEN_CACHE
            .lock()
            .unwrap()
            .insert(installation_id, (minted.clone(), expires));
    }
    minted
}

/// Drop a cached installation token (disconnect / revocation).
pub fn clear_github_token(installation_id: &str) {
    GITHUB_TOKEN_CACHE
        .lock()
        .unwrap()
        .remove(installation_id.trim());
}

/// Best-effort: delete this user's relay routing rows for one installation so
/// the cloud stops pushing its events.
pub async fn github_disconnect_installation(
    settings: &SettingsManager,
    config: &Config,
    installation_id: &str,
) {
    clear_github_token(installation_id);
    let Some(token) = fresh_access_token(settings, config).await else {
        return;
    };
    if let Ok(c) = client() {
        let _ = c
            .post(format!(
                "{}/v1/relay/github/disconnect",
                cloud_base_url(config).trim_end_matches('/')
            ))
            .json(&json!({"installation_id": installation_id.trim()}))
            .bearer_auth(&token)
            .send();
    }
}

/// Best-effort: delete this user's relay routing row for one workspace so the
/// cloud stops pushing its events.
pub async fn slack_disconnect_workspace(
    settings: &SettingsManager,
    config: &Config,
    team_id: &str,
) {
    let Some(token) = fresh_access_token(settings, config).await else {
        return;
    };
    if let Ok(c) = client() {
        let _ = c
            .post(format!(
                "{}/v1/relay/slack/uninstall",
                cloud_base_url(config).trim_end_matches('/')
            ))
            .json(&json!({"team_id": team_id}))
            .bearer_auth(&token)
            .send();
    }
}

// ---------------------------------------------------------------------------
// persona gallery (J3)
// ---------------------------------------------------------------------------

async fn gallery_get(
    settings: &SettingsManager,
    config: &Config,
    path: &str,
) -> Option<Value> {
    let token = fresh_access_token(settings, config).await?;
    let resp = client()
        .ok()?
        .get(format!(
            "{}{path}",
            cloud_base_url(config).trim_end_matches('/')
        ))
        .bearer_auth(&token)
        .send()
        .ok()?;
    if resp.status().is_success() {
        resp.json().ok()
    } else {
        None
    }
}

/// Curated persona cards visible to this user's tenant; None when signed out
/// or the cloud is unreachable (gallery requires sign-in by design).
pub async fn gallery_list(settings: &SettingsManager, config: &Config) -> Option<Value> {
    gallery_get(settings, config, "/v1/personas/gallery").await
}

pub async fn gallery_manifest(
    settings: &SettingsManager,
    config: &Config,
    slug: &str,
) -> Option<Value> {
    gallery_get(settings, config, &format!("/v1/personas/gallery/{slug}/manifest")).await
}

/// Best-effort product telemetry (slug only, no content).
pub async fn gallery_install_event(settings: &SettingsManager, config: &Config, slug: &str) {
    let Some(token) = fresh_access_token(settings, config).await else {
        return;
    };
    let body = json!({"platform": std::env::consts::OS});
    if let Ok(c) = client() {
        let _ = c
            .post(format!(
                "{}/v1/personas/gallery/{slug}/install-events",
                cloud_base_url(config).trim_end_matches('/')
            ))
            .json(&body)
            .bearer_auth(&token)
            .send();
    }
}

/// Solo-page payload: the cloud card, with the manifest markdown surfaced raw.
/// The local capability derivation (consent summary + recommends) plugs in with
/// the persona manifest parser (阶段 L2).
pub async fn gallery_detail(
    settings: &SettingsManager,
    config: &Config,
    slug: &str,
) -> Option<Value> {
    let card = gallery_get(settings, config, &format!("/v1/personas/gallery/{slug}")).await?;
    let manifest = gallery_manifest(settings, config, slug).await?;
    Some(json!({
        "ok": true,
        "card": card,
        "manifest_markdown": manifest.get("manifest_markdown").cloned().unwrap_or(json!("")),
        "capabilities": json!([]),
        "recommends": json!([]),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_roundtrip() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn urlencode_reserved() {
        assert_eq!(urlencode("https://a.io/x?y=1"), "https%3A%2F%2Fa.io%2Fx%3Fy%3D1");
        assert_eq!(urlencode("openid profile"), "openid%20profile");
    }

    #[test]
    fn provider_map() {
        assert_eq!(provider_for("gmail"), Some("google"));
        assert_eq!(provider_for("slack"), Some("slack"));
        assert_eq!(provider_for("nope"), None);
    }

    #[test]
    fn managed_state_single_use() {
        assert!(!consume_managed_state(""));
        assert!(!consume_managed_state("never-registered"));
        PENDING_MANAGED
            .lock()
            .unwrap()
            .insert("s1".into(), now());
        assert!(consume_managed_state("s1"));
        assert!(!consume_managed_state("s1")); // consumed exactly once
    }

    #[test]
    fn profile_from_callback_fields() {
        let mut form = Map::new();
        form.insert("access_token".into(), json!("tok"));
        form.insert("refresh_token".into(), json!("ref"));
        form.insert("account".into(), json!("a@b.c"));
        form.insert("account_id".into(), json!("123"));
        form.insert("expires_in".into(), json!(3600));
        let p = managed_profile_from_callback(&form);
        assert_eq!(p["type"], "oauth");
        assert_eq!(p["managed"], true);
        assert_eq!(p["access_token"], "tok");
        assert_eq!(p["account_id"], "123");
        assert_eq!(p["expires"].as_u64().unwrap(), now() + 3600 - 60);
        // absent expires_in ⇒ no expires field (non-expiring token)
        let mut bare = Map::new();
        bare.insert("access_token".into(), json!("t"));
        let p2 = managed_profile_from_callback(&bare);
        assert!(p2.get("expires").is_none());
    }
}
