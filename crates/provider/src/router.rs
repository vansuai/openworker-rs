//! Provider router — dispatches to the right client by `provider:` prefix.
//!
//! Mirrors `coworker/providers/router.py`.

use crate::anthropic::AnthropicClient;
use crate::bedrock::BedrockClient;
use crate::error::Error;
use crate::openai::OpenAiClient;
use crate::registry::{self, ProviderConfig};
use crate::types::{AssistantTurn, ModelCapabilities, StreamEvent};
use crate::vertex::VertexClient;
use std::collections::HashMap;
use std::sync::Arc;

/// One completed response or a streaming iterator.
#[allow(dead_code)]
pub enum ProviderResponse {
    Turn(AssistantTurn),
    Stream(Box<dyn Iterator<Item = StreamEvent> + Send>),
}

impl ProviderResponse {
    #[allow(dead_code)]
    pub fn into_turn(self) -> Result<AssistantTurn, Error> {
        match self {
            Self::Turn(t) => Ok(t),
            Self::Stream(_) => Err(Error::Other("Cannot convert stream to turn".into())),
        }
    }
}

/// The provider trait — single-shot completions.
pub trait Provider: Send + Sync {
    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<AssistantTurn, Error>;

    fn stream(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent, Error>> + Send>, Error> {
        let turn = self.complete(model, messages, tools, settings)?;
        Ok(Box::new(std::iter::once(Ok(StreamEvent::Turn { turn }))))
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities;

    fn name(&self) -> &str;

    /// Receive updated secrets from the server layer.  The default is a no-op;
    /// [`Router`] overrides this to maintain a per-provider profile cache so that
    /// API keys are injected into every model call.
    fn update_secrets(&self, _providers: HashMap<String, serde_json::Value>) {}
}

/// A router that dispatches by `provider:` prefix.
pub struct Router {
    default: String,
    clients: std::sync::Mutex<HashMap<String, Arc<dyn Provider>>>,
    /// Secrets from the server layer — maps "provider:{name}" to its config object.
    /// Updated via [`Self::update_secrets`] whenever secrets are loaded or changed.
    secrets: std::sync::RwLock<HashMap<String, serde_json::Value>>,
}

impl Router {
    pub fn new(default_provider: &str) -> Self {
        Self {
            default: default_provider.to_string(),
            clients: std::sync::Mutex::new(HashMap::new()),
            secrets: std::sync::RwLock::new(HashMap::new()),
        }
    }

    fn bare(&self, model: &str) -> (String, String) {
        if let Some(idx) = model.find(':') {
            let prefix = &model[..idx];
            if registry::get_descriptor(prefix).is_some() {
                return (model[idx + 1..].to_string(), prefix.to_string());
            }
        }
        (model.to_string(), self.default.clone())
    }

    /// Look up the provider profile from stored secrets (mirrors Python's
    /// `self._secrets.get(f"provider:{name}")`).
    fn resolve_profile(&self, name: &str) -> ProviderConfig {
        self.secrets
            .read()
            .unwrap()
            .get(&format!("provider:{name}"))
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_or_build(&self, name: &str, profile: &ProviderConfig) -> Arc<dyn Provider> {
        {
            let clients = self.clients.lock().unwrap();
            if let Some(c) = clients.get(name) {
                return Arc::clone(c);
            }
        }

        let client: Arc<dyn Provider> = match name {
            "openai" | "ollama" | "openrouter" | "deepseek" | "gemini" | "kimi" | "minimax"
            | "xai" | "mistral" | "together" | "fireworks" | "zai" | "qwen" | "meta" => {
                let base_url = registry::openai_base_url(profile);
                let api_key =
                    registry::resolve_api_key(None, None, profile).unwrap_or("placeholder");
                Arc::new(OpenAiClient::new(
                    base_url,
                    api_key.to_string(),
                    name.to_string(),
                ))
            }
            "anthropic" => {
                let api_key = registry::resolve_api_key(None, None, profile)
                    .unwrap_or("")
                    .to_string();
                Arc::new(AnthropicClient::new(
                    api_key,
                    "claude-sonnet-4-6".into(),
                    8192,
                ))
            }
            "bedrock" => Arc::new(BedrockClient::new(profile)),
            "vertex" => Arc::new(VertexClient::new(profile)),
            _ => {
                let base_url = registry::openai_base_url(profile);
                let api_key = registry::resolve_api_key(None, None, profile)
                    .unwrap_or("placeholder")
                    .to_string();
                Arc::new(OpenAiClient::new(base_url, api_key, name.to_string()))
            }
        };

        let mut clients = self.clients.lock().unwrap();
        clients.insert(name.to_string(), Arc::clone(&client));
        client
    }

    pub fn invalidate(&self, name: Option<&str>) {
        let mut clients = self.clients.lock().unwrap();
        match name {
            Some(n) => {
                clients.remove(n);
            }
            None => {
                clients.clear();
            }
        }
    }
}

impl Provider for Router {
    fn update_secrets(&self, providers: HashMap<String, serde_json::Value>) {
        *self.secrets.write().unwrap() = providers;
        self.invalidate(None);
    }

    fn complete(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<AssistantTurn, Error> {
        let (bare, provider) = self.bare(model);
        let profile = self.resolve_profile(&provider);
        let client = self.get_or_build(&provider, &profile);
        client.complete(&bare, messages, tools, settings)
    }

    fn stream(
        &self,
        model: &str,
        messages: Vec<serde_json::Value>,
        tools: Option<Vec<serde_json::Value>>,
        settings: serde_json::Value,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent, Error>> + Send>, Error> {
        let (bare, provider) = self.bare(model);
        let profile = self.resolve_profile(&provider);
        let client = self.get_or_build(&provider, &profile);
        // Delegate to the concrete client's stream (SSE deltas), not complete().
        client.stream(&bare, messages, tools, settings)
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        use crate::openai;
        let (bare, provider) = self.bare(model);
        match provider.as_str() {
            "anthropic" => crate::anthropic::capabilities_for(&bare),
            "bedrock" => crate::bedrock::capabilities_for(&bare),
            "vertex" => crate::vertex::capabilities_for(&bare),
            _ => openai::capabilities_for(&bare),
        }
    }

    fn name(&self) -> &str {
        "router"
    }
}
