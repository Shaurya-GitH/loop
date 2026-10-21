//! OpenAI-compatible custom provider builder.

use super::openai_compatible::{openai_compatible_provider, ModelDefaults, OpenAiCompatibleConfig};
use crate::models::Provider;
use crate::types::{
    InputModality, Model, ModelCost, OpenAICompletionsCompat, API_OPENAI_COMPLETIONS,
};

/// Spec for a model registered on a custom OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct CustomModelSpec {
    /// Model id.
    pub id: String,
    /// Display name (defaults to id).
    pub name: Option<String>,
    /// Supports reasoning.
    pub reasoning: bool,
    /// Input modalities (defaults to text).
    pub input: Option<Vec<InputModality>>,
    /// Pricing (defaults to zero).
    pub cost: Option<ModelCost>,
    /// Context window (default 128_000).
    pub context_window: Option<u64>,
    /// Max output tokens (default 16_384).
    pub max_tokens: Option<u64>,
    /// Compat overrides.
    pub compat: Option<OpenAICompletionsCompat>,
}

impl CustomModelSpec {
    /// Create a minimal text model spec.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            reasoning: false,
            input: None,
            cost: None,
            context_window: None,
            max_tokens: None,
            compat: None,
        }
    }

    /// Enable reasoning.
    pub fn with_reasoning(mut self, reasoning: bool) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Set the display name.
    pub fn with_name(mut self, name: Option<&str>) -> Self {
        self.name = name.map(String::from);
        self
    }

    /// Set the context window (tokens).
    pub fn with_context_window(mut self, context_window: Option<u64>) -> Self {
        self.context_window = context_window;
        self
    }

    /// Set the max output tokens.
    pub fn with_max_tokens(mut self, max_tokens: Option<u64>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set compat overrides.
    pub fn with_compat(mut self, compat: OpenAICompletionsCompat) -> Self {
        self.compat = Some(compat);
        self
    }
}

/// Configuration for an OpenAI-compatible custom provider.
#[derive(Debug, Clone)]
pub struct CustomProviderConfig {
    /// Provider id (e.g. `ollama`, `vllm`, `my-gateway`).
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Base URL including version path if needed (e.g. `http://localhost:11434/v1`).
    pub base_url: String,
    /// Env vars to try for the API key. Empty / omitted → keyless.
    pub api_key_env: Vec<String>,
    /// Pinned models (optional; the rest are listed from `/models`).
    pub models: Vec<CustomModelSpec>,
    /// Default headers.
    pub headers: Option<std::collections::HashMap<String, String>>,
}

impl CustomModelSpec {
    fn into_model(self, provider_id: &str, base_url: &str) -> Model {
        let defaults = ModelDefaults::default();
        Model {
            name: self.name.unwrap_or_else(|| self.id.clone()),
            id: self.id,
            api: API_OPENAI_COMPLETIONS.to_string(),
            provider: provider_id.into(),
            base_url: base_url.into(),
            reasoning: self.reasoning,
            thinking_level_map: None,
            input: self.input.unwrap_or_else(|| vec![InputModality::Text]),
            cost: self.cost.unwrap_or_default(),
            context_window: self.context_window.unwrap_or(defaults.context_window),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            headers: None,
            compat: self.compat,
        }
    }
}

/// Build a custom OpenAI-compatible provider. Listed models come from its `/models`
/// endpoint; `models` are pinned (always shown, and their metadata wins).
pub fn custom_provider(config: CustomProviderConfig) -> Provider {
    let pinned = config
        .models
        .into_iter()
        .map(|spec| spec.into_model(&config.id, &config.base_url))
        .collect();
    openai_compatible_provider(OpenAiCompatibleConfig {
        name: config.name.unwrap_or_else(|| config.id.clone()),
        id: config.id,
        base_url: config.base_url,
        api_key_env: config.api_key_env,
        headers: config.headers,
        defaults: ModelDefaults::default(),
        pinned,
        fallback: vec![],
        model_filter: None,
    })
}
