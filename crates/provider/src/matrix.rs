//! Curated model matrix — the only models we actively suggest and vouch for.
//!
//! Mirrors `coworker/providers/matrix.py`.

use std::collections::HashMap;

use crate::types::ModelCapabilities;

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub label: &'static str,
    pub caps: ModelCapabilities,
    pub context_window: Option<u32>,
}

// Capabilities constants — written as literal struct expressions so they can be
// used in a const context (const fn is not available without nightly).
const AGENTIC: ModelCapabilities = ModelCapabilities {
    tools: true,
    vision: false,
    pdf: false,
    parallel_tool_calls: true,
    streaming: true,
};

const AGENTIC_VISION: ModelCapabilities = ModelCapabilities {
    tools: true,
    vision: true,
    pdf: true,
    parallel_tool_calls: true,
    streaming: true,
};

const CUSTOM_VISION: ModelCapabilities = ModelCapabilities {
    tools: true,
    vision: true,
    pdf: false,
    parallel_tool_calls: true,
    streaming: true,
};

const NEMOTRON: ModelCapabilities = ModelCapabilities {
    tools: true,
    vision: false,
    pdf: false,
    parallel_tool_calls: false,
    streaming: true,
};

pub const MATRIX: &[(&str, ModelEntry)] = &[
    // -- first-party -----------------------------------------------------------
    (
        "gpt-5.6-sol",
        ModelEntry {
            label: "GPT-5.6 Sol · OpenAI",
            caps: AGENTIC_VISION,
            context_window: Some(400_000),
        },
    ),
    (
        "gpt-5.6-terra",
        ModelEntry {
            label: "GPT-5.6 Terra · OpenAI",
            caps: AGENTIC_VISION,
            context_window: Some(400_000),
        },
    ),
    (
        "gpt-5.6-luna",
        ModelEntry {
            label: "GPT-5.6 Luna · OpenAI",
            caps: AGENTIC_VISION,
            context_window: Some(400_000),
        },
    ),
    (
        "gpt-5.5",
        ModelEntry {
            label: "GPT-5.5 · OpenAI",
            caps: AGENTIC_VISION,
            context_window: Some(400_000),
        },
    ),
    (
        "anthropic:claude-fable-5",
        ModelEntry {
            label: "Claude Fable 5 · Anthropic",
            caps: AGENTIC_VISION,
            context_window: Some(1_000_000),
        },
    ),
    (
        "anthropic:claude-opus-4-8",
        ModelEntry {
            label: "Claude Opus 4.8 · Anthropic",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "anthropic:claude-sonnet-4-6",
        ModelEntry {
            label: "Claude Sonnet 4.6 · Anthropic",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "anthropic:claude-haiku-4-5",
        ModelEntry {
            label: "Claude Haiku 4.5 · Anthropic",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "gemini:gemini-3.1-pro-preview",
        ModelEntry {
            label: "Gemini 3.1 Pro · Google",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    (
        "gemini:gemini-3.6-flash",
        ModelEntry {
            label: "Gemini 3.6 Flash · Google",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    (
        "gemini:gemini-2.5-pro",
        ModelEntry {
            label: "Gemini 2.5 Pro · Google",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    (
        "gemini:gemini-2.5-flash",
        ModelEntry {
            label: "Gemini 2.5 Flash · Google",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    // -- direct OpenAI-compatible vendors -------------------------------------
    (
        "meta:muse-spark-1.1",
        ModelEntry {
            label: "Muse Spark 1.1 · Meta",
            caps: CUSTOM_VISION,
            context_window: None,
        },
    ),
    (
        "zai:glm-5.2",
        ModelEntry {
            label: "GLM-5.2 · Z AI",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "deepseek:deepseek-v4-flash",
        ModelEntry {
            label: "DeepSeek V4 Flash · DeepSeek",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "deepseek:deepseek-v4-pro",
        ModelEntry {
            label: "DeepSeek V4 Pro · DeepSeek",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "kimi:kimi-k2.6",
        ModelEntry {
            label: "Kimi K2.6 · Moonshot",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "minimax:MiniMax-M2.5",
        ModelEntry {
            label: "MiniMax M2.5 · MiniMax",
            caps: AGENTIC,
            context_window: None,
        },
    ),
    (
        "qwen:qwen3-max",
        ModelEntry {
            label: "Qwen3 Max · Alibaba",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "xai:grok-4.3",
        ModelEntry {
            label: "Grok 4.3 · xAI",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "mistral:mistral-large-latest",
        ModelEntry {
            label: "Mistral Large · Mistral",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    // -- resellers -----------------------------------------------------------
    (
        "together:thinkingmachines/Inkling",
        ModelEntry {
            label: "Inkling · via Together",
            caps: AGENTIC,
            context_window: None,
        },
    ),
    (
        "together:zai-org/GLM-5.2",
        ModelEntry {
            label: "GLM-5.2 · via Together",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "together:moonshotai/Kimi-K3",
        ModelEntry {
            label: "Kimi K3 · via Together",
            caps: CUSTOM_VISION,
            context_window: Some(1_000_000),
        },
    ),
    (
        "together:moonshotai/Kimi-K2.7-Code",
        ModelEntry {
            label: "Kimi K2.7 Code · via Together",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "together:moonshotai/Kimi-K2.6",
        ModelEntry {
            label: "Kimi K2.6 · via Together",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "together:deepseek-ai/DeepSeek-V4-Pro",
        ModelEntry {
            label: "DeepSeek V4 Pro · via Together",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "together:meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8",
        ModelEntry {
            label: "Llama 4 Maverick · via Together",
            caps: AGENTIC,
            context_window: Some(1_000_000),
        },
    ),
    (
        "fireworks:accounts/fireworks/models/glm-5p2",
        ModelEntry {
            label: "GLM-5.2 · via Fireworks",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "fireworks:accounts/fireworks/models/kimi-k2p6",
        ModelEntry {
            label: "Kimi K2.6 · via Fireworks",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "fireworks:accounts/fireworks/models/deepseek-v4-pro",
        ModelEntry {
            label: "DeepSeek V4 Pro · via Fireworks",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "fireworks:accounts/fireworks/models/llama4-maverick-instruct-basic",
        ModelEntry {
            label: "Llama 4 Maverick · via Fireworks",
            caps: AGENTIC,
            context_window: Some(1_000_000),
        },
    ),
    (
        "openrouter:z-ai/glm-5.2",
        ModelEntry {
            label: "GLM-5.2 · via OpenRouter",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "openrouter:moonshotai/kimi-k2.6",
        ModelEntry {
            label: "Kimi K2.6 · via OpenRouter",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
    (
        "openrouter:deepseek/deepseek-v4-pro",
        ModelEntry {
            label: "DeepSeek V4 Pro · via OpenRouter",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "openrouter:meta-llama/llama-4-maverick",
        ModelEntry {
            label: "Llama 4 Maverick · via OpenRouter",
            caps: AGENTIC,
            context_window: Some(1_000_000),
        },
    ),
    // -- cloud accounts -------------------------------------------------------
    (
        "bedrock:claude/anthropic.claude-sonnet-4-6-v1:0",
        ModelEntry {
            label: "Claude Sonnet 4.6 · AWS Bedrock",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "bedrock:claude/anthropic.claude-haiku-4-5-v1:0",
        ModelEntry {
            label: "Claude Haiku 4.5 · AWS Bedrock",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "bedrock:other/amazon.nova-2-pro-v1:0",
        ModelEntry {
            label: "Nova 2 Pro · AWS Bedrock",
            caps: AGENTIC,
            context_window: Some(300_000),
        },
    ),
    (
        "bedrock:other/meta.llama4-maverick-17b-instruct-v1:0",
        ModelEntry {
            label: "Llama 4 Maverick · AWS Bedrock",
            caps: AGENTIC,
            context_window: Some(1_000_000),
        },
    ),
    (
        "bedrock:other/mistral.mistral-large-3-v1:0",
        ModelEntry {
            label: "Mistral Large 3 · AWS Bedrock",
            caps: AGENTIC,
            context_window: Some(128_000),
        },
    ),
    (
        "bedrock:other/nvidia.nemotron-super-3-120b",
        ModelEntry {
            label: "Nemotron Super 3 120B · AWS Bedrock",
            caps: NEMOTRON,
            context_window: None,
        },
    ),
    (
        "vertex:gemini/gemini-3.1-pro-preview",
        ModelEntry {
            label: "Gemini 3.1 Pro · Vertex AI",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    (
        "vertex:gemini/gemini-3.6-flash",
        ModelEntry {
            label: "Gemini 3.6 Flash · Vertex AI",
            caps: AGENTIC_VISION,
            context_window: Some(1_048_576),
        },
    ),
    (
        "vertex:claude/claude-sonnet-4-6",
        ModelEntry {
            label: "Claude Sonnet 4.6 · Vertex AI",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "vertex:claude/claude-haiku-4-5",
        ModelEntry {
            label: "Claude Haiku 4.5 · Vertex AI",
            caps: AGENTIC_VISION,
            context_window: Some(200_000),
        },
    ),
    (
        "vertex:openweight/meta/llama-4-maverick-17b-128e-instruct-maas",
        ModelEntry {
            label: "Llama 4 Maverick · Vertex AI",
            caps: AGENTIC,
            context_window: Some(1_000_000),
        },
    ),
    (
        "vertex:openweight/qwen/qwen3-coder-480b-a35b-instruct-maas",
        ModelEntry {
            label: "Qwen3 Coder · Vertex AI",
            caps: AGENTIC,
            context_window: Some(256_000),
        },
    ),
];

/// Full-id → display-label map.
pub fn model_labels() -> HashMap<String, String> {
    MATRIX
        .iter()
        .map(|(k, e)| (k.to_string(), e.label.to_string()))
        .collect()
}

/// Full-id → context-window map (verified entries only).
pub fn model_context_windows() -> HashMap<String, u32> {
    MATRIX
        .iter()
        .filter_map(|(k, e)| e.context_window.map(|w| (k.to_string(), w)))
        .collect()
}

/// BARE model ids the matrix curates for a provider (prefix stripped).
/// OpenAI entries are stored without a prefix (bare ids route to OpenAI default).
pub fn models_for_provider(provider: &str) -> Vec<String> {
    if provider == "openai" {
        return MATRIX
            .iter()
            .filter(|(k, _)| !k.contains(':'))
            .map(|(k, _)| k.to_string())
            .collect();
    }
    let prefix = format!("{provider}:");
    MATRIX
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, _)| k[prefix.len()..].to_string())
        .collect()
}

/// Look up a matrix entry by full model id.
pub fn entry_for(model: &str) -> Option<&'static ModelEntry> {
    MATRIX.iter().find(|(k, _)| *k == model).map(|(_, e)| e)
}
