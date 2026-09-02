//! Friendly translation of model access + quota failures.
//!
//! Mirrors `coworker/providers/errors.py` — maps terse provider errors to one
//! actionable sentence. Unrecognized errors return `None` so callers surface the raw text.

const NO_ACCESS: &[&str] = &[
    "model_not_found",
    "does not exist or you do not have access",
    "does not have access to model",
    "permission_error",
    "permission denied",
];

const NO_QUOTA: &[&str] = &[
    "insufficient_quota",
    "exceeded your current quota",
    "credit balance is too low",
    "billing hard limit",
    "insufficient balance",
];

/// One actionable sentence for quota/access failures, or `None` to keep the raw error.
pub fn friendly_model_error(model: &str, err: &str) -> Option<String> {
    let text = err.to_lowercase();
    let no_access = format!(
        "Your account doesn't have access to {model} — new models can roll out \
         gradually or require a plan upgrade. Pick a different model, or check \
         the provider's console for availability."
    );
    if NO_QUOTA.iter().any(|m| text.contains(m)) {
        return Some(format!(
            "Your account is out of quota for {model} — add credits or raise the limit \
             in the provider's billing console, or pick a different model."
        ));
    }
    if text.contains("messages is empty") || text.contains("(2013)") {
        return Some(format!(
            "The model provider rejected the request because the message list was empty for \
             {model}. This is usually a client formatting bug — retry after restarting the app."
        ));
    }
    if text.contains("invalid api key")
        && text.contains("2049")
        && model.starts_with("minimax:")
    {
        return Some(format!(
            "MiniMax rejected the API key for {model}. Subscription keys (sk-cp-…) use a \
             different API than pay-as-you-go keys (sk-api-…). If you use a subscription key, \
             leave the endpoint blank (auto-routes to the Anthropic API) or set \
             https://api.minimax.cn/anthropic for China. Otherwise use a pay-as-you-go \
             sk-api- key with https://api.minimax.io/v1."
        ));
    }
    if NO_ACCESS.iter().any(|m| text.contains(m)) {
        return Some(no_access);
    }
    let bare = model.split(':').next_back().unwrap_or(model).to_lowercase();
    if text.contains("not_found_error") && text.contains(&format!("model: {bare}")) {
        return Some(no_access);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_insufficient_balance_maps_to_quota_message() {
        let raw = "API error: unknown_error — Insufficient Balance";
        let friendly = friendly_model_error("deepseek:deepseek-v4-flash", raw).unwrap();
        assert!(friendly.contains("out of quota"));
        assert!(friendly.contains("deepseek:deepseek-v4-flash"));
    }
}
