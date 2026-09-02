//! Provider descriptor registry + factory.
//!
//! Mirrors `coworker/providers/registry.py`.
//! Providers are registered here; the Router resolves them at runtime.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Descriptor types
// ---------------------------------------------------------------------------

/// One config field for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderField {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default = "true_val")]
    pub required: bool,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub choices: Vec<serde_json::Value>,
    #[serde(default)]
    pub endpoint_help: String,
}

fn true_val() -> bool {
    true
}

/// Provider descriptor: its config fields + a factory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub needs_key: bool,
    #[serde(default)]
    pub fields: Vec<ProviderField>,
    #[serde(default)]
    pub recommended_model: Option<String>,
    #[serde(default)]
    pub env_key: Option<String>,
    #[serde(default)]
    pub blurb: String,
}

impl ProviderDescriptor {
    pub fn to_dict(&self) -> Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

// ---------------------------------------------------------------------------
// Known providers
// ---------------------------------------------------------------------------

/// MiniMax subscription (Token Plan) keys use the Anthropic-compatible API.
pub fn minimax_uses_anthropic_protocol(api_key: &str) -> bool {
    api_key.starts_with("sk-cp-")
}

/// Anthropic-protocol base URL for MiniMax (subscription keys).
pub fn minimax_anthropic_base_url(profile: &ProviderConfig) -> String {
    if let Some(url) = profile.get("base_url").and_then(|v| v.as_str()) {
        let u = url.trim().trim_end_matches('/');
        if u.contains("/anthropic") {
            return u.to_string();
        }
        if u.contains("minimax.cn") {
            return "https://api.minimax.cn/anthropic".to_string();
        }
    }
    "https://api.minimax.io/anthropic".to_string()
}

/// OpenAI-compatible base URL: profile override, then vendor default, then OpenAI.
pub fn openai_base_url(provider: &str, profile: &ProviderConfig) -> String {
    profile
        .get("base_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| default_base_url_for(provider))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
}

/// Resolve API key from explicit value, env var, or provider config.
pub fn resolve_api_key<'a>(
    explicit: Option<&'a str>,
    env: Option<&'a str>,
    profile: &'a ProviderConfig,
) -> Option<&'a str> {
    explicit
        .filter(|s| !s.is_empty())
        .or_else(|| env.filter(|s| !s.is_empty()))
        .or_else(|| {
            profile
                .get("api_key")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
}

/// Returns the default base URL for a known OpenAI-compatible provider, or None.
pub fn default_base_url_for(name: &str) -> Option<String> {
    match name {
        "deepseek" => Some("https://api.deepseek.com/v1".to_string()),
        "kimi" => Some("https://api.moonshot.ai/v1".to_string()),
        "minimax" => Some("https://api.minimax.io/v1".to_string()),
        "qwen" => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1".to_string()),
        "xai" => Some("https://api.x.ai/v1".to_string()),
        "mistral" => Some("https://api.mistral.ai/v1".to_string()),
        "together" => Some("https://api.together.xyz/v1".to_string()),
        "fireworks" => Some("https://api.fireworks.ai/inference/v1".to_string()),
        "openrouter" => Some("https://openrouter.ai/api/v1".to_string()),
        "zai" => Some("https://api.z.ai/api/paas/v4".to_string()),
        "meta" => Some("https://api.meta.ai/v1".to_string()),
        "ollama" => Some("http://localhost:11434/v1".to_string()),
        "openai" => Some("https://api.openai.com/v1".to_string()),
        _ => None,
    }
}

/// Normalize an Ollama URL to its OpenAI-compatible /v1 endpoint.
#[allow(dead_code)]
pub fn normalize_ollama_url(url: Option<&str>) -> String {
    let base = url
        .unwrap_or("http://localhost:11434")
        .trim()
        .trim_end_matches('/');
    if base.is_empty() {
        return "http://localhost:11434/v1".to_string();
    }
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// Resolve AWS credentials from a profile config dict.
#[allow(dead_code)]
pub fn resolve_aws(profile: &ProviderConfig) -> AwsCredentials<'_> {
    let auth_method = profile
        .get("auth_method")
        .and_then(|v| v.as_str())
        .unwrap_or("api_key");
    let region = profile
        .get("region")
        .and_then(|v| v.as_str())
        .unwrap_or("us-east-1");
    let bedrock_key = profile.get("bedrock_api_key").and_then(|v| v.as_str());
    let aws_profile = profile.get("aws_profile").and_then(|v| v.as_str());
    let access_key = profile.get("aws_access_key_id").and_then(|v| v.as_str());
    let secret_key = profile
        .get("aws_secret_access_key")
        .and_then(|v| v.as_str());
    let session_token = profile.get("aws_session_token").and_then(|v| v.as_str());
    AwsCredentials {
        auth_method,
        region,
        bedrock_key,
        aws_profile,
        access_key,
        secret_key,
        session_token,
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct AwsCredentials<'a> {
    pub auth_method: &'a str,
    pub region: &'a str,
    pub bedrock_key: Option<&'a str>,
    pub aws_profile: Option<&'a str>,
    pub access_key: Option<&'a str>,
    pub secret_key: Option<&'a str>,
    pub session_token: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Config type alias
// ---------------------------------------------------------------------------

/// A provider configuration dict (from SecretStore / stored profile).
pub type ProviderConfig = serde_json::Map<String, Value>;

// ---------------------------------------------------------------------------
// All known descriptors
// ---------------------------------------------------------------------------

pub fn all_descriptors() -> Vec<ProviderDescriptor> {
    vec![
        ProviderDescriptor {
            name: "openai".into(),
            title: "OpenAI".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "OpenAI API key".into(), secret: true, ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("gpt-5.6-sol".into()),
            env_key: Some("OPENAI_API_KEY".into()),
            blurb: "OpenAI's official API".into(),
        },
        ProviderDescriptor {
            name: "anthropic".into(),
            title: "Claude (Anthropic)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Anthropic API key".into(), secret: true, placeholder: "sk-ant-…".into(), ..Default::default() },
            ],
            recommended_model: Some("claude-sonnet-4-6".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            blurb: "Anthropic's Messages API".into(),
        },
        ProviderDescriptor {
            name: "gemini".into(),
            title: "Gemini (Google)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Gemini API key".into(), secret: true, placeholder: "AIza…".into(), ..Default::default() },
            ],
            recommended_model: Some("gemini-3.6-flash".into()),
            env_key: Some("GEMINI_API_KEY".into()),
            blurb: "Google's GenAI API".into(),
        },
        ProviderDescriptor {
            name: "ollama".into(),
            title: "Ollama (local models)".into(),
            needs_key: false,
            fields: vec![
                ProviderField { key: "base_url".into(), label: "Ollama server URL".into(), required: false, placeholder: "http://localhost:11434".into(), ..Default::default() },
            ],
            recommended_model: Some("qwen3-coder:30b".into()),
            env_key: None,
            blurb: "Local models via Ollama".into(),
        },
        // -- OpenAI-compatible vendors -------------------------------------------
        ProviderDescriptor {
            name: "zai".into(),
            title: "Z AI (GLM)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Z AI API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), endpoint_help: "Prefilled with Z AI's international endpoint. China mainland: https://open.bigmodel.cn/api/paas/v4".into(), ..Default::default() },
            ],
            recommended_model: Some("glm-5.2".into()),
            env_key: Some("ZAI_API_KEY".into()),
            blurb: "Z AI's GLM models".into(),
        },
        ProviderDescriptor {
            name: "deepseek".into(),
            title: "DeepSeek".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "DeepSeek API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("deepseek-v4-flash".into()),
            env_key: Some("DEEPSEEK_API_KEY".into()),
            blurb: "DeepSeek's official API".into(),
        },
        ProviderDescriptor {
            name: "kimi".into(),
            title: "Kimi (Moonshot AI)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Moonshot API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), endpoint_help: "Prefilled with Moonshot's international endpoint. China mainland: https://api.moonshot.cn/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("kimi-k2.6".into()),
            env_key: Some("MOONSHOT_API_KEY".into()),
            blurb: "Moonshot AI's Kimi models".into(),
        },
        ProviderDescriptor {
            name: "minimax".into(),
            title: "MiniMax".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "MiniMax API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), endpoint_help: "Pay-as-you-go (sk-api-…): https://api.minimax.io/v1 (intl) or https://api.minimax.cn/v1 (China). Subscription (sk-cp-…): auto-routes to the Anthropic API — set https://api.minimax.cn/anthropic for China.".into(), ..Default::default() },
            ],
            recommended_model: Some("MiniMax-M2.5".into()),
            env_key: Some("MINIMAX_API_KEY".into()),
            blurb: "MiniMax's official API".into(),
        },
        ProviderDescriptor {
            name: "qwen".into(),
            title: "Qwen (Alibaba)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Qwen API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), endpoint_help: "Prefilled with Alibaba Model Studio's international endpoint. China (Beijing): https://dashscope.aliyuncs.com/compatible-mode/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("qwen3-max".into()),
            env_key: Some("DASHSCOPE_API_KEY".into()),
            blurb: "Alibaba's Qwen models".into(),
        },
        ProviderDescriptor {
            name: "xai".into(),
            title: "xAI (Grok)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "xAI API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("grok-4.3".into()),
            env_key: Some("XAI_API_KEY".into()),
            blurb: "xAI's Grok models".into(),
        },
        ProviderDescriptor {
            name: "mistral".into(),
            title: "Mistral".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Mistral API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), ..Default::default() },
            ],
            recommended_model: Some("mistral-large-latest".into()),
            env_key: Some("MISTRAL_API_KEY".into()),
            blurb: "Mistral's official API".into(),
        },
        ProviderDescriptor {
            name: "meta".into(),
            title: "Meta (Muse Spark)".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Meta API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "base_url".into(), label: "Custom endpoint".into(), required: false, placeholder: "https://…/openai/v1".into(), endpoint_help: "Prefilled with the Meta Model API endpoint (public preview, US-only as of 2026-07).".into(), ..Default::default() },
            ],
            recommended_model: Some("muse-spark-1.1".into()),
            env_key: Some("META_API_KEY".into()),
            blurb: "Meta's Muse Spark model".into(),
        },
        // -- Resellers -----------------------------------------------------------
        ProviderDescriptor {
            name: "together".into(),
            title: "Together AI".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Together AI API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
            ],
            recommended_model: Some("zai-org/GLM-5.2".into()),
            env_key: Some("TOGETHER_API_KEY".into()),
            blurb: "Many labs' models via Together AI".into(),
        },
        ProviderDescriptor {
            name: "fireworks".into(),
            title: "Fireworks AI".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "Fireworks AI API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
            ],
            recommended_model: Some("accounts/fireworks/models/glm-5p2".into()),
            env_key: Some("FIREWORKS_API_KEY".into()),
            blurb: "Many labs' models via Fireworks AI".into(),
        },
        ProviderDescriptor {
            name: "openrouter".into(),
            title: "OpenRouter".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "api_key".into(), label: "OpenRouter API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
            ],
            recommended_model: Some("z-ai/glm-5.2".into()),
            env_key: Some("OPENROUTER_API_KEY".into()),
            blurb: "Many labs' models via OpenRouter".into(),
        },
        ProviderDescriptor {
            name: "bedrock".into(),
            title: "AWS Bedrock".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "bedrock_api_key".into(), label: "Bedrock credentials".into(), secret: true, placeholder: "access_key:secret_key".into(), help: "access_key:secret_key".into(), ..Default::default() },
                ProviderField { key: "region".into(), label: "AWS region".into(), required: false, placeholder: "us-east-1".into(), ..Default::default() },
            ],
            recommended_model: Some("anthropic.claude-sonnet-4-6-v1:0".into()),
            env_key: None,
            blurb: "Claude and open-weight models via AWS Bedrock".into(),
        },
        ProviderDescriptor {
            name: "vertex".into(),
            title: "Google Vertex AI".into(),
            needs_key: true,
            fields: vec![
                ProviderField { key: "vertex_api_key".into(), label: "Google API key".into(), secret: true, placeholder: "…".into(), ..Default::default() },
                ProviderField { key: "region".into(), label: "GCP region".into(), required: false, placeholder: "us-central1".into(), ..Default::default() },
            ],
            recommended_model: Some("gemini-3.6-flash".into()),
            env_key: None,
            blurb: "Gemini and Claude via Google Vertex AI".into(),
        },
    ]
}

pub fn get_descriptor(name: &str) -> Option<&'static ProviderDescriptor> {
    static DESCRIPTORS: std::sync::OnceLock<Vec<ProviderDescriptor>> = std::sync::OnceLock::new();
    DESCRIPTORS.get_or_init(all_descriptors);
    DESCRIPTORS
        .get()
        .and_then(|ds| ds.iter().find(|d| d.name == name))
}

impl Default for ProviderField {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            secret: false,
            required: true,
            help: String::new(),
            placeholder: String::new(),
            default: String::new(),
            choices: Vec::new(),
            endpoint_help: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn minimax_default_base_url_not_openai() {
        let profile = Map::new();
        assert_eq!(
            openai_base_url("minimax", &profile),
            "https://api.minimax.io/v1"
        );
    }

    #[test]
    fn profile_base_url_overrides_vendor_default() {
        let mut profile = Map::new();
        profile.insert(
            "base_url".into(),
            Value::String("https://custom.example/v1".into()),
        );
        assert_eq!(
            openai_base_url("minimax", &profile),
            "https://custom.example/v1"
        );
    }

    #[test]
    fn minimax_subscription_key_uses_anthropic_protocol() {
        assert!(minimax_uses_anthropic_protocol("sk-cp-abc"));
        assert!(!minimax_uses_anthropic_protocol("sk-api-abc"));
    }

    #[test]
    fn minimax_anthropic_base_url_defaults_international() {
        assert_eq!(
            minimax_anthropic_base_url(&Map::new()),
            "https://api.minimax.io/anthropic"
        );
    }

    #[test]
    fn minimax_anthropic_base_url_china_from_profile_host() {
        let mut profile = Map::new();
        profile.insert(
            "base_url".into(),
            Value::String("https://api.minimax.cn/v1".into()),
        );
        assert_eq!(
            minimax_anthropic_base_url(&profile),
            "https://api.minimax.cn/anthropic"
        );
    }
}
