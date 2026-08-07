//! Per-account connector profiles — mirrors `coworker/connectors/accounts.py`,
//! `gmail_accounts.py`, `gcal_accounts.py` and `hubspot_portals.py`.
//!
//! The shape is shared: `<connector>:account:<id>` holds one account's tokens,
//! `<connector>:default` holds only the default-account pointer + connector-wide
//! policy (gmail filters, hubspot hidden-fields) — never tokens.
//!
//! All helpers are async because the underlying `SettingsManager` secrets are
//! tokio `RwLock`-backed; handlers call them directly.

use crate::state::SettingsManager;
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Generic account layer (accounts.py)
// ---------------------------------------------------------------------------

pub const IDENTITY: &str = "@identity";

/// `<connector>:account:` prefix.
pub fn prefix(connector: &str) -> String {
    format!("{connector}:account:")
}

/// `<connector>:default` key.
pub fn default_key(connector: &str) -> String {
    format!("{connector}:default")
}

/// Emails want case-folding; UUIDs/numeric ids are unaffected by it.
pub fn norm(value: &Value) -> String {
    value
        .as_str()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default()
}

fn norm_str(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Connectors with a per-account profile layout (batch-2 connectors adopt this
/// via the generic routes). gmail/gcal/hubspot keep their bespoke routes.
pub fn is_account_connector(name: &str) -> bool {
    matches!(name, "github" | "notion" | "attio" | "posthog" | "linear")
}

/// `(account_id, profile_key, profile)` for the requested — or default — account.
/// Profile is `None` when nothing matches.
pub async fn resolve(
    settings: &SettingsManager,
    connector: &str,
    account: &str,
) -> (String, String, Option<Map<String, Value>>) {
    let account_id = norm_str(account);
    let account_id = if account_id.is_empty() {
        default_account(settings, connector).await
    } else {
        account_id
    };
    if account_id.is_empty() {
        return (String::new(), String::new(), None);
    }
    let key = format!("{}{account_id}", prefix(connector));
    let profile = settings.secrets_get(&key).await;
    (account_id, key, profile)
}

/// Every connected account as `(id, profile)` sorted by id.
pub async fn list_accounts(
    settings: &SettingsManager,
    connector: &str,
) -> Vec<(String, Map<String, Value>)> {
    let pre = prefix(connector);
    let all = settings.secrets_all().await;
    let mut out: Vec<_> = all
        .into_iter()
        .filter(|(k, _)| k.starts_with(&pre))
        .map(|(k, v)| (k[pre.len()..].to_string(), v))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The default account id: the stored pointer if it still exists, else the
/// first connected account, else "".
pub async fn default_account(settings: &SettingsManager, connector: &str) -> String {
    let accounts = list_accounts(settings, connector).await;
    let pointer = settings
        .secrets_get(&default_key(connector))
        .await
        .and_then(|m| m.get("default_account").cloned())
        .map(|v| norm(&v))
        .unwrap_or_default();
    if accounts.iter().any(|(id, _)| *id == pointer) {
        return pointer;
    }
    accounts.into_iter().next().map(|(id, _)| id).unwrap_or_default()
}

/// Store one account; the first connected account becomes the default.
/// Re-adding an id replaces its credentials in place.
pub async fn add_account(
    settings: &SettingsManager,
    connector: &str,
    account_id: &str,
    profile: Map<String, Value>,
) -> Value {
    let account_id = norm_str(account_id);
    if account_id.is_empty() {
        return json!({"ok": false, "error": "account id missing"});
    }
    settings
        .secrets_put(&format!("{}{account_id}", prefix(connector)), profile.clone())
        .await;
    let mut pointer = settings.secrets_get(&default_key(connector)).await.unwrap_or_default();
    pointer
        .entry("default_account")
        .or_insert_with(|| Value::String(account_id.clone()));
    pointer
        .entry("type")
        .or_insert_with(|| profile.get("type").cloned().unwrap_or(json!("token")));
    pointer
        .entry("enabled")
        .or_insert(Value::Bool(true));
    settings.secrets_put(&default_key(connector), pointer).await;
    json!({"ok": true, "account": account_id})
}

/// Point the default pointer at one connected account.
pub async fn set_default(
    settings: &SettingsManager,
    connector: &str,
    account_id: &str,
    pointer_field: &str,
) -> Value {
    let account_id = norm_str(account_id);
    let key = format!("{}{account_id}", prefix(connector));
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    let mut pointer = settings.secrets_get(&default_key(connector)).await.unwrap_or_default();
    pointer.insert(pointer_field.into(), Value::String(account_id.clone()));
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(&default_key(connector), pointer).await;
    let mut resp = Map::new();
    resp.insert("ok".into(), Value::Bool(true));
    resp.insert(pointer_field.to_string(), Value::String(account_id));
    json!(resp)
}

/// Drop one account. The default pointer moves to the next account; removing
/// the last account removes the pointer profile too.
pub async fn disconnect_account(
    settings: &SettingsManager,
    connector: &str,
    account_id: &str,
    pointer_field: &str,
    keep_pointer_if: fn(&Map<String, Value>) -> bool,
) -> Value {
    let account_id = norm_str(account_id);
    let key = format!("{}{account_id}", prefix(connector));
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    settings.secrets_delete(&key).await;
    let remaining = list_accounts(settings, connector).await;
    let mut pointer = settings.secrets_get(&default_key(connector)).await.unwrap_or_default();
    let is_default = pointer.get(pointer_field).map(norm) == Some(account_id);
    if is_default {
        if let Some((next, _)) = remaining.first() {
            pointer.insert(pointer_field.into(), Value::String(next.clone()));
            settings.secrets_put(&default_key(connector), pointer).await;
        } else {
            pointer.remove(pointer_field);
            if keep_pointer_if(&pointer) {
                settings.secrets_put(&default_key(connector), pointer).await;
            } else {
                settings.secrets_delete(&default_key(connector)).await;
            }
        }
    }
    json!({"ok": true, "remaining_accounts": remaining.len()})
}

/// Same as [`disconnect_account`] but with an explicit profile prefix + pointer
/// key (for the bespoke gmail/gcal/hubspot layouts).
pub async fn disconnect_with_prefix(
    settings: &SettingsManager,
    pre: &str,
    def_key: &str,
    account_id: &str,
    pointer_field: &str,
    keep_pointer_if: fn(&Map<String, Value>) -> bool,
) -> Value {
    let account_id = norm_str(account_id);
    let key = format!("{pre}{account_id}");
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    settings.secrets_delete(&key).await;
    let remaining = list_with(settings, pre).await;
    let mut pointer = settings.secrets_get(def_key).await.unwrap_or_default();
    let is_default = pointer.get(pointer_field).map(norm) == Some(account_id);
    if is_default {
        if let Some((next, _)) = remaining.first() {
            pointer.insert(pointer_field.into(), Value::String(next.clone()));
            settings.secrets_put(def_key, pointer).await;
        } else {
            pointer.remove(pointer_field);
            if keep_pointer_if(&pointer) {
                settings.secrets_put(def_key, pointer).await;
            } else {
                settings.secrets_delete(def_key).await;
            }
        }
    }
    json!({"ok": true, "remaining_accounts": remaining.len()})
}

pub(crate) fn keep_none(_p: &Map<String, Value>) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Gmail (gmail_accounts.py)
// ---------------------------------------------------------------------------

pub const GMAIL_PREFIX: &str = "gmail:account:";
pub const GMAIL_DEFAULT: &str = "gmail:default";

/// `(email, profile_key, profile)` for the requested — or default — mailbox.
pub async fn gmail_resolve(
    settings: &SettingsManager,
    account: &str,
) -> (String, String, Option<Map<String, Value>>) {
    resolve_with(settings, account, "gmail", GMAIL_PREFIX, GMAIL_DEFAULT, "default_account").await
}

async fn resolve_with(
    settings: &SettingsManager,
    account: &str,
    _connector: &str,
    pre: &str,
    def_key: &str,
    pointer_field: &str,
) -> (String, String, Option<Map<String, Value>>) {
    let email = norm_str(account);
    let email = if email.is_empty() {
        let accounts = list_with(settings, pre).await;
        let pointer = settings
            .secrets_get(def_key)
            .await
            .and_then(|m| m.get(pointer_field).cloned())
            .map(|v| norm(&v))
            .unwrap_or_default();
        if accounts.iter().any(|(id, _)| *id == pointer) {
            pointer
        } else {
            accounts.into_iter().next().map(|(id, _)| id).unwrap_or_default()
        }
    } else {
        email
    };
    if email.is_empty() {
        return (String::new(), String::new(), None);
    }
    let key = format!("{pre}{email}");
    let profile = settings.secrets_get(&key).await;
    (email, key, profile)
}

async fn list_with(
    settings: &SettingsManager,
    pre: &str,
) -> Vec<(String, Map<String, Value>)> {
    let all = settings.secrets_all().await;
    let mut out: Vec<_> = all
        .into_iter()
        .filter(|(k, _)| k.starts_with(pre))
        .map(|(k, v)| (k[pre.len()..].to_string(), v))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Drop one mailbox; the default pointer moves on. Removing the last account
/// keeps the filters (they're policy, not credentials) unless there are none.
pub async fn gmail_disconnect(settings: &SettingsManager, email: &str) -> Value {
    let email = norm_str(email);
    let key = format!("{GMAIL_PREFIX}{email}");
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    settings.secrets_delete(&key).await;
    let remaining = list_with(settings, GMAIL_PREFIX).await;
    let mut pointer = settings.secrets_get(GMAIL_DEFAULT).await.unwrap_or_default();
    let is_default = pointer.get("default_account").map(norm) == Some(email.clone());
    if is_default {
        if let Some((next, _)) = remaining.first() {
            pointer.insert("default_account".into(), Value::String(next.clone()));
            settings.secrets_put(GMAIL_DEFAULT, pointer).await;
        } else {
            pointer.remove("default_account");
            pointer.remove("managed");
            if pointer.get("filters").map(|v| v.is_object()).unwrap_or(false) {
                settings.secrets_put(GMAIL_DEFAULT, pointer).await;
            } else {
                settings.secrets_delete(GMAIL_DEFAULT).await;
            }
        }
    }
    json!({"ok": true, "remaining_accounts": remaining.len()})
}

/// Point the default mailbox pointer.
pub async fn gmail_set_default(settings: &SettingsManager, email: &str) -> Value {
    let email = norm_str(email);
    let key = format!("{GMAIL_PREFIX}{email}");
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    let mut pointer = settings.secrets_get(GMAIL_DEFAULT).await.unwrap_or_default();
    pointer.insert("default_account".into(), Value::String(email.clone()));
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(GMAIL_DEFAULT, pointer).await;
    json!({"ok": true, "default_account": email})
}

/// The "Never show agents" filters: `{senders: [...], labels: [...]}`.
pub async fn gmail_get_filters(settings: &SettingsManager) -> Value {
    let f = settings
        .secrets_get(GMAIL_DEFAULT)
        .await
        .and_then(|m| m.get("filters").cloned())
        .unwrap_or(json!({}));
    json!({
        "senders": f.get("senders").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        "labels": f.get("labels").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
    })
}

/// Replace either list (`None` = leave unchanged).
pub async fn gmail_set_filters(
    settings: &SettingsManager,
    senders: Option<Vec<Value>>,
    labels: Option<Vec<Value>>,
) -> Value {
    let mut current = gmail_get_filters(settings).await;
    if let Some(senders) = senders {
        let mut cleaned: Vec<String> = senders
            .iter()
            .filter_map(|v| v.as_str())
            .map(norm_str)
            .filter(|s| !s.is_empty())
            .collect();
        cleaned.sort();
        cleaned.dedup();
        current["senders"] = json!(cleaned);
    }
    if let Some(labels) = labels {
        let mut cleaned: Vec<String> = labels
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        cleaned.sort();
        cleaned.dedup();
        current["labels"] = json!(cleaned);
    }
    let mut pointer = settings.secrets_get(GMAIL_DEFAULT).await.unwrap_or_default();
    pointer.insert("filters".into(), current.clone());
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(GMAIL_DEFAULT, pointer).await;
    json!({"ok": true, "filters": current})
}

/// `addr@x.com` = exact; `@domain.com` = that domain (suffix on the addr).
pub fn sender_matches(address: &str, rules: &[String]) -> bool {
    let address = address.trim().to_lowercase();
    if address.is_empty() {
        return false;
    }
    rules.iter().any(|rule| {
        let rule = rule.trim().to_lowercase();
        if rule.is_empty() {
            return false;
        }
        if let Some(domain) = rule.strip_prefix('@') {
            address.ends_with(&format!("@{domain}")) || address.ends_with(&domain)
        } else {
            address == rule
        }
    })
}

// ---------------------------------------------------------------------------
// Google Calendar (gcal_accounts.py)
// ---------------------------------------------------------------------------

pub const GCAL_PREFIX: &str = "google_calendar:account:";
pub const GCAL_DEFAULT: &str = "google_calendar:default";

pub async fn gcal_set_default(settings: &SettingsManager, email: &str) -> Value {
    let email = norm_str(email);
    let key = format!("{GCAL_PREFIX}{email}");
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "account not connected"});
    }
    let mut pointer = settings.secrets_get(GCAL_DEFAULT).await.unwrap_or_default();
    pointer.insert("default_account".into(), Value::String(email.clone()));
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(GCAL_DEFAULT, pointer).await;
    json!({"ok": true, "default_account": email})
}

/// Drop one account; the last one removes the pointer profile too (no
/// account-wide policy to preserve, unlike gmail's filters).
pub async fn gcal_disconnect(settings: &SettingsManager, email: &str) -> Value {
    disconnect_with_prefix(settings, GCAL_PREFIX, GCAL_DEFAULT, email, "default_account", keep_none).await
}

// ---------------------------------------------------------------------------
// HubSpot (hubspot_portals.py)
// ---------------------------------------------------------------------------

pub const HUBSPOT_PREFIX: &str = "hubspot:portal:";
pub const HUBSPOT_DEFAULT: &str = "hubspot:default";

pub async fn hubspot_set_default(settings: &SettingsManager, hub_id: &str) -> Value {
    let hub_id = hub_id.trim().to_string();
    let key = format!("{HUBSPOT_PREFIX}{hub_id}");
    if settings.secrets_get(&key).await.is_none() {
        return json!({"ok": false, "error": "portal not connected"});
    }
    let mut pointer = settings.secrets_get(HUBSPOT_DEFAULT).await.unwrap_or_default();
    pointer.insert("default_portal".into(), Value::String(hub_id.clone()));
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(HUBSPOT_DEFAULT, pointer).await;
    json!({"ok": true, "default_portal": hub_id})
}

/// Drop one portal; the default pointer moves on. Removing the last portal
/// keeps hidden_fields (policy, not credentials) unless there are none.
pub async fn hubspot_disconnect(settings: &SettingsManager, hub_id: &str) -> Value {
    disconnect_with_prefix(settings, HUBSPOT_PREFIX, HUBSPOT_DEFAULT, hub_id.trim(), "default_portal", keep_none).await
}

/// The hidden-fields denylist (property names stripped from every record agents
/// read — model-facing policy, not a human ACL).
pub async fn hubspot_get_hidden_fields(settings: &SettingsManager) -> Value {
    json!(settings
        .secrets_get(HUBSPOT_DEFAULT)
        .await
        .and_then(|m| m.get("hidden_fields").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default())
}

pub async fn hubspot_set_hidden_fields(
    settings: &SettingsManager,
    fields: Vec<Value>,
) -> Value {
    let mut cleaned: Vec<String> = fields
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    cleaned.sort();
    cleaned.dedup();
    let mut pointer = settings.secrets_get(HUBSPOT_DEFAULT).await.unwrap_or_default();
    pointer.insert("hidden_fields".into(), json!(cleaned));
    pointer.entry("type").or_insert(json!("oauth"));
    pointer.entry("enabled").or_insert(Value::Bool(true));
    settings.secrets_put(HUBSPOT_DEFAULT, pointer).await;
    json!({"ok": true, "hidden_fields": cleaned})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_settings() -> SettingsManager {
        let dir = std::env::temp_dir().join(format!(
            "ocw-accounts-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SettingsManager::new is private; build through a tiny wrapper below.
        SettingsManager::open_for_tests(dir)
    }

    #[tokio::test]
    async fn add_default_and_resolve() {
        let s = temp_settings();
        let profile = json!({"type": "token", "token": "abc", "account": "Alice"}).as_object().unwrap().clone();
        let r = add_account(&s, "github", "alice", profile).await;
        assert_eq!(r["ok"], true);
        assert_eq!(default_account(&s, "github").await, "alice");
        let (_id, key, profile) = resolve(&s, "github", "").await;
        assert_eq!(key, "github:account:alice");
        assert_eq!(profile.unwrap()["token"], "abc");
    }

    #[tokio::test]
    async fn disconnect_moves_pointer() {
        let s = temp_settings();
        let p1 = json!({"type": "token", "account": "a"}).as_object().unwrap().clone();
        let p2 = json!({"type": "token", "account": "b"}).as_object().unwrap().clone();
        add_account(&s, "github", "a", p1).await;
        add_account(&s, "github", "b", p2).await;
        let r = disconnect_account(&s, "github", "a", "default_account", keep_none).await;
        assert_eq!(r["ok"], true);
        assert_eq!(default_account(&s, "github").await, "b");
    }

    #[tokio::test]
    async fn gmail_filters_roundtrip() {
        let s = temp_settings();
        let p = json!({"type": "oauth", "access_token": "t", "account": "a@b.c"})
            .as_object()
            .unwrap()
            .clone();
        s.secrets_put(&format!("{GMAIL_PREFIX}a@b.c"), p).await;
        let r = gmail_set_filters(&s, Some(vec![json!("B@x.com"), json!("@corp.io")]), Some(vec![json!("Work")]))
            .await;
        assert_eq!(r["ok"], true);
        let f = gmail_get_filters(&s).await;
        // senders are normalized (lowercased), sorted and deduped
        assert_eq!(f["senders"], json!(["@corp.io", "b@x.com"]));
        assert_eq!(f["labels"], json!(["Work"]));
        assert!(sender_matches("x@corp.io", &["@corp.io".to_string()]));
        assert!(!sender_matches("x@corp.io", &["@other.io".to_string()]));
    }

    #[tokio::test]
    async fn hubspot_hidden_fields_roundtrip() {
        let s = temp_settings();
        let r = hubspot_set_hidden_fields(&s, vec![json!("SSN"), json!("Internal")]).await;
        assert_eq!(r["ok"], true);
        let f = hubspot_get_hidden_fields(&s).await;
        assert_eq!(f, json!(["internal", "ssn"]));
    }
}
