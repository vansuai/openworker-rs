//! Browser OAuth for remote MCP servers (OAuth 2.1 + PKCE + Dynamic Client Registration).
//!
//! Mirrors `coworker/mcp/oauth.py`: tokens live in the secrets store under
//! `mcp-oauth:{server}`, the loopback route resolves a single-slot pending future,
//! and interactive browser sign-in is explicit-connect-only.

use crate::state::SettingsManager;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;

pub const PROFILE_PREFIX: &str = "mcp-oauth:";
pub const CALLBACK_PATH: &str = "/mcp/oauth/callback";
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_NAME: &str = "OpenWorker";

static LAST_AUTHORIZE_URL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static PENDING: OnceLock<Mutex<Option<PendingFlow>>> = OnceLock::new();
static EXCHANGE: OnceLock<Mutex<Option<TokenExchangeParams>>> = OnceLock::new();

fn last_url_lock() -> &'static Mutex<Option<String>> {
    LAST_AUTHORIZE_URL.get_or_init(|| Mutex::new(None))
}

fn pending_lock() -> &'static Mutex<Option<PendingFlow>> {
    PENDING.get_or_init(|| Mutex::new(None))
}

fn exchange_lock() -> &'static Mutex<Option<TokenExchangeParams>> {
    EXCHANGE.get_or_init(|| Mutex::new(None))
}

struct PendingFlow {
    tx: oneshot::Sender<(String, Option<String>)>,
    expected_state: Option<String>,
}

#[derive(Clone)]
struct TokenExchangeParams {
    server_name: String,
    token_endpoint: String,
    client_id: String,
    redirect_uri: String,
    code_verifier: String,
}

#[derive(Debug, Clone)]
struct OAuthMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

fn profile_key(name: &str) -> String {
    format!("{PROFILE_PREFIX}{name}")
}

pub fn redirect_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn redirect_uri(port: u16) -> String {
    format!("{}{CALLBACK_PATH}", redirect_base(port))
}

pub async fn last_authorize_url() -> Option<String> {
    last_url_lock().lock().await.clone()
}

pub async fn has_tokens(settings: &SettingsManager, name: &str) -> bool {
    settings
        .secrets_get(&profile_key(name))
        .await
        .and_then(|m| m.get("tokens").cloned())
        .and_then(|v| v.get("access_token").and_then(|t| t.as_str()).map(String::from))
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Bearer header for an OAuth MCP server when tokens are stored.
pub async fn auth_headers(
    settings: &SettingsManager,
    name: &str,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(token) = access_token(settings, name).await {
        out.insert("Authorization".into(), format!("Bearer {token}"));
    }
    out
}

async fn access_token(settings: &SettingsManager, name: &str) -> Option<String> {
    settings
        .secrets_get(&profile_key(name))
        .await
        .and_then(|m| m.get("tokens").cloned())
        .and_then(|v| v.get("access_token").and_then(|t| t.as_str()).map(String::from))
        .filter(|s| !s.is_empty())
}

fn pkce_pair() -> (String, String) {
    let verifier = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

fn oauth_state() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn state_from_url(url: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    for part in query.split('&') {
        let mut kv = part.splitn(2, '=');
        if kv.next()? == "state" {
            return kv.next().map(|s| s.to_string());
        }
    }
    None
}

/// Resolve the pending flow with `(code, state)`. Returns false when nothing waits
/// or the state does not match.
pub async fn deliver_callback(code: &str, state: Option<&str>) -> bool {
    let mut guard = pending_lock().lock().await;
    let Some(flow) = guard.as_ref() else {
        return false;
    };
    if let Some(expected) = &flow.expected_state {
        if state != Some(expected.as_str()) {
            return false;
        }
    }
    let flow = guard.take().expect("flow present");
    let _ = flow.tx.send((code.to_string(), state.map(String::from)));
    true
}

async fn discover_metadata(server_url: &str) -> Result<OAuthMetadata, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let parsed = reqwest::Url::parse(server_url).map_err(|e| format!("bad mcp url: {e}"))?;
    let origin = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or("")
    );

    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server"),
        format!("{server_url}/.well-known/oauth-protected-resource"),
        format!("{origin}/.well-known/oauth-protected-resource"),
    ];

    for url in &candidates {
        if let Ok(resp) = client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            if !resp.status().is_success() {
                continue;
            }
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            if let Some(md) = metadata_from_value(&body) {
                return Ok(md);
            }
            if let Some(as_url) = body
                .get("authorization_servers")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
            {
                if let Ok(resp) = client
                    .get(as_url)
                    .header("Accept", "application/json")
                    .send()
                    .await
                {
                    if resp.status().is_success() {
                        let body: Value = resp.json().await.unwrap_or(Value::Null);
                        if let Some(md) = metadata_from_value(&body) {
                            return Ok(md);
                        }
                    }
                }
            }
        }
    }
    Err("could not discover OAuth metadata for MCP server".into())
}

fn metadata_from_value(v: &Value) -> Option<OAuthMetadata> {
    let auth = v.get("authorization_endpoint")?.as_str()?.to_string();
    let token = v.get("token_endpoint")?.as_str()?.to_string();
    let reg = v
        .get("registration_endpoint")
        .and_then(|r| r.as_str())
        .map(String::from);
    Some(OAuthMetadata {
        authorization_endpoint: auth,
        token_endpoint: token,
        registration_endpoint: reg,
    })
}

async fn dcr_client_id(metadata: &OAuthMetadata, redirect: &str) -> Result<String, String> {
    let Some(reg_url) = metadata.registration_endpoint.as_deref() else {
        return Err("OAuth server does not support dynamic client registration".into());
    };
    let body = json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(reg_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("DCR request failed: {e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("DCR failed: {text}"));
    }
    let v: Value = resp.json().await.map_err(|e| format!("DCR json: {e}"))?;
    v.get("client_id")
        .and_then(|c| c.as_str())
        .map(String::from)
        .ok_or_else(|| "DCR response missing client_id".into())
}

async fn load_or_register_client(
    settings: &SettingsManager,
    server_name: &str,
    metadata: &OAuthMetadata,
    redirect: &str,
) -> Result<String, String> {
    if let Some(existing) = settings.secrets_get(&profile_key(server_name)).await {
        if let Some(id) = existing
            .get("client_info")
            .and_then(|c| c.get("client_id"))
            .and_then(|v| v.as_str())
        {
            return Ok(id.to_string());
        }
    }
    let client_id = dcr_client_id(metadata, redirect).await?;
    let mut patch = Map::new();
    patch.insert(
        "client_info".into(),
        json!({
            "client_id": client_id,
            "redirect_uris": [redirect],
        }),
    );
    merge_profile(settings, server_name, patch).await;
    Ok(client_id)
}

async fn merge_profile(settings: &SettingsManager, server_name: &str, patch: Map<String, Value>) {
    let key = profile_key(server_name);
    let mut current = settings.secrets_get(&key).await.unwrap_or_default();
    for (k, v) in patch {
        current.insert(k, v);
    }
    settings.secrets_put(&key, current).await;
}

async fn exchange_code(settings: &SettingsManager, params: &TokenExchangeParams, code: &str) -> Result<(), String> {
    let body = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", params.redirect_uri.as_str()),
        ("client_id", params.client_id.as_str()),
        ("code_verifier", params.code_verifier.as_str()),
    ];
    let client = reqwest::Client::new();
    let resp = client
        .post(&params.token_endpoint)
        .form(&body)
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange HTTP error: {text}"));
    }
    let tokens: Value = resp
        .json()
        .await
        .map_err(|e| format!("token response json: {e}"))?;
    let mut patch = Map::new();
    patch.insert("tokens".into(), tokens);
    patch.insert(
        "tokens_issued_at".into(),
        json!(chrono::Utc::now().timestamp()),
    );
    merge_profile(settings, &params.server_name, patch).await;
    Ok(())
}

fn build_authorize_url(
    metadata: &OAuthMetadata,
    client_id: &str,
    redirect: &str,
    state: &str,
    challenge: &str,
) -> String {
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        metadata.authorization_endpoint,
        pct_encode(client_id),
        pct_encode(redirect),
        pct_encode(state),
        pct_encode(challenge),
    )
}

fn pct_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Run the full interactive OAuth flow when no tokens are stored yet.
/// Returns the authorize URL (empty when tokens already exist).
pub async fn interactive_connect(
    settings: &SettingsManager,
    server_name: &str,
    server_url: &str,
    port: u16,
) -> Result<String, String> {
    if has_tokens(settings, server_name).await {
        return Ok(String::new());
    }

    let metadata = discover_metadata(server_url).await?;
    let redirect = redirect_uri(port);
    let client_id = load_or_register_client(settings, server_name, &metadata, &redirect).await?;
    let (verifier, challenge) = pkce_pair();
    let state = oauth_state();
    let authorize_url = build_authorize_url(&metadata, &client_id, &redirect, &state, &challenge);

    let (tx, rx) = oneshot::channel();
    {
        let mut pending = pending_lock().lock().await;
        if pending.is_some() {
            return Err("another MCP sign-in is already in progress".into());
        }
        *pending = Some(PendingFlow {
            tx,
            expected_state: Some(state),
        });
        *exchange_lock().lock().await = Some(TokenExchangeParams {
            server_name: server_name.to_string(),
            token_endpoint: metadata.token_endpoint,
            client_id,
            redirect_uri: redirect,
            code_verifier: verifier,
        });
    }

    *last_url_lock().lock().await = Some(authorize_url.clone());
    open_browser(&authorize_url);

    let (code, _state) = match timeout(FLOW_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            pending_lock().lock().await.take();
            exchange_lock().lock().await.take();
            return Err("sign-in flow was cancelled".into());
        }
        Err(_) => {
            pending_lock().lock().await.take();
            exchange_lock().lock().await.take();
            return Err(
                "sign-in timed out — the browser window was not completed in 5 minutes".into(),
            );
        }
    };

    let exchange = exchange_lock()
        .lock()
        .await
        .take()
        .ok_or_else(|| "OAuth exchange state missing".to_string())?;
    exchange_code(settings, &exchange, &code).await?;
    Ok(authorize_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_from_authorize_url() {
        let url = "https://idp.example/authorize?client_id=x&state=abc123&scope=y";
        assert_eq!(state_from_url(url), Some("abc123".to_string()));
        assert_eq!(
            state_from_url("https://idp.example/authorize?client_id=x"),
            None
        );
    }

    #[tokio::test]
    async fn deliver_without_waiter_rejected() {
        assert!(!deliver_callback("code", Some("state")).await);
    }

    #[tokio::test]
    async fn deliver_rejects_mismatched_state() {
        let (tx, mut rx) = oneshot::channel();
        *pending_lock().lock().await = Some(PendingFlow {
            tx,
            expected_state: Some("good".into()),
        });
        assert!(!deliver_callback("code", Some("bad")).await);
        assert!(!rx.try_recv().is_ok());
        assert!(deliver_callback("code", Some("good")).await);
        assert_eq!(rx.await.unwrap(), ("code".to_string(), Some("good".to_string())));
    }
}
