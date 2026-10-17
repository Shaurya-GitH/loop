//! Built-in OpenAI-compatible providers offered by `/login`.

use std::collections::HashMap;

use super::openai_compatible::{
    openai_compatible_provider, ModelDefaults, ModelFilter, OpenAiCompatibleConfig,
};
use super::soket::{
    soket_seed_models, SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_PROVIDER_ID, SOKET_PROVIDER_NAME,
};
use crate::models::Provider;
use crate::types::Model;

/// OpenRouter provider id.
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
/// OpenAI provider id.
pub const OPENAI_PROVIDER_ID: &str = "openai";

/// A provider Loop knows how to connect to with just an API key.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    /// Provider id (settings, credentials, `provider/model` specs).
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One-line description for the `/login` picker.
    pub description: &'static str,
    /// OpenAI-compatible base URL.
    pub base_url: &'static str,
    /// Env vars checked for the API key, first wins.
    pub api_key_envs: &'static [&'static str],
    /// Extra request headers.
    pub headers: &'static [(&'static str, &'static str)],
    /// What a key looks like, for prompts (e.g. `sk-or-…`).
    pub key_hint: &'static str,
    /// Lead-in shown before `key_url` in the login prompt (e.g. `get one at`).
    pub key_cta: &'static str,
    /// Where to create a key.
    pub key_url: &'static str,
    /// Defaults for listed models without metadata.
    pub defaults: ModelDefaults,
    /// Keeps chat-capable model ids.
    pub model_filter: Option<ModelFilter>,
    /// Models shown until the first successful listing.
    pub fallback_models: Option<fn() -> Vec<Model>>,
    /// Authenticated endpoint (relative to `base_url`) used to verify a key, for
    /// providers whose `/models` is public.
    pub key_check_path: Option<&'static str>,
}

impl ProviderPreset {
    /// Build the provider.
    pub fn provider(&self) -> Provider {
        openai_compatible_provider(self.config())
    }

    /// Reject a key the provider doesn't accept, where `/models` can't tell.
    pub async fn verify_key(&self, api_key: &str) -> Result<(), String> {
        match self.key_check_path {
            Some(path) => crate::api::verify_api_key(self.base_url, path, api_key)
                .await
                .map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    fn config(&self) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            id: self.id.into(),
            name: self.name.into(),
            base_url: self.base_url.into(),
            api_key_env: self.api_key_envs.iter().map(|s| (*s).into()).collect(),
            headers: (!self.headers.is_empty()).then(|| {
                self.headers
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect::<HashMap<String, String>>()
            }),
            defaults: self.defaults,
            pinned: vec![],
            fallback: self.fallback_models.map(|f| f()).unwrap_or_default(),
            model_filter: self.model_filter,
        }
    }
}

/// OpenAI's `/models` lists every model family; keep the ones chat completions serve.
fn openai_chat_model(id: &str) -> bool {
    const CHAT_PREFIXES: &[&str] = &["gpt-", "o1", "o3", "o4", "chatgpt-"];
    const NOT_CHAT: &[&str] = &[
        "audio",
        "realtime",
        "tts",
        "transcribe",
        "image",
        "search",
        "embedding",
        "moderation",
        "instruct",
    ];
    CHAT_PREFIXES.iter().any(|p| id.starts_with(p)) && !NOT_CHAT.iter().any(|n| id.contains(n))
}

/// OpenRouter's `/models` lists `:batch` variants that only its Batch API serves; drop them.
fn openrouter_chat_model(id: &str) -> bool {
    !id.ends_with(":batch")
}

/// Presets in `/login` order.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: SOKET_PROVIDER_ID,
        name: SOKET_PROVIDER_NAME,
        description: "Soket's inference Studio",
        base_url: SOKET_BASE_URL,
        api_key_envs: SOKET_API_KEY_ENVS,
        headers: &[],
        key_hint: "your Soket API key",
        key_cta: "get your free credits at",
        key_url: "http://console.soket.ai/",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: true,
        },
        model_filter: None,
        fallback_models: Some(soket_seed_models),
        key_check_path: None,
    },
    ProviderPreset {
        id: OPENROUTER_PROVIDER_ID,
        name: "OpenRouter",
        description: "Hundreds of models with one key",
        base_url: "https://openrouter.ai/api/v1",
        api_key_envs: &["OPENROUTER_API_KEY"],
        headers: &[
            ("HTTP-Referer", "https://github.com/soketlabs/loop"),
            ("X-Title", "Loop"),
        ],
        key_hint: "sk-or-…",
        key_cta: "get one at",
        key_url: "https://openrouter.ai/keys",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        },
        model_filter: Some(openrouter_chat_model),
        fallback_models: None,
        key_check_path: Some("/key"),
    },
    ProviderPreset {
        id: OPENAI_PROVIDER_ID,
        name: "OpenAI",
        description: "Chat models",
        base_url: "https://api.openai.com/v1",
        api_key_envs: &["OPENAI_API_KEY"],
        headers: &[],
        key_hint: "sk-…",
        key_cta: "get one at",
        key_url: "https://platform.openai.com/api-keys",
        defaults: ModelDefaults {
            context_window: 128_000,
            max_tokens: 16_384,
            reasoning: false,
        },
        model_filter: Some(openai_chat_model),
        fallback_models: None,
        key_check_path: None,
    },
];

/// Look up a preset by id (case-insensitive).
pub fn provider_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_in_login_order_with_soket_first() {
        let ids: Vec<_> = PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids, ["soket", "openrouter", "openai"]);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(provider_preset(" OpenRouter ").unwrap().id, "openrouter");
        assert!(provider_preset("groq").is_none());
    }

    #[test]
    fn openrouter_sends_attribution_headers() {
        let provider = provider_preset("openrouter").unwrap().provider();
        let headers = provider.headers.as_ref().unwrap();
        assert_eq!(headers["X-Title"], "Loop");
        assert!(headers.contains_key("HTTP-Referer"));
        assert!(provider.is_dynamic());
        assert!(
            provider.get_models().is_empty(),
            "no fallback models for OpenRouter"
        );
    }

    #[test]
    fn soket_keeps_its_seed_until_listed() {
        let provider = provider_preset("soket").unwrap().provider();
        assert_eq!(provider.id, SOKET_PROVIDER_ID);
        assert!(!provider.get_models().is_empty());
    }

    #[test]
    fn openai_filter_keeps_chat_models_only() {
        for keep in [
            "gpt-4o",
            "gpt-4.1-mini",
            "o3",
            "o4-mini",
            "chatgpt-4o-latest",
        ] {
            assert!(openai_chat_model(keep), "{keep}");
        }
        for drop in [
            "text-embedding-3-small",
            "whisper-1",
            "dall-e-3",
            "tts-1",
            "gpt-4o-realtime-preview",
            "gpt-4o-audio-preview",
            "gpt-image-1",
            "omni-moderation-latest",
            "gpt-3.5-turbo-instruct",
        ] {
            assert!(!openai_chat_model(drop), "{drop}");
        }
    }

    #[test]
    fn openrouter_filter_drops_batch_variants() {
        let keep = provider_preset("openrouter")
            .unwrap()
            .model_filter
            .expect("openrouter has a model filter");
        for id in [
            "deepseek/deepseek-v4.1-flash",
            "z-ai/glm-5.2:free",
            "anthropic/claude-sonnet-5.5",
        ] {
            assert!(keep(id), "{id}");
        }
        for id in [
            "deepseek/deepseek-v4.1-flash:batch",
            "anthropic/claude-sonnet-5.5:batch",
        ] {
            assert!(!keep(id), "{id}");
        }
    }
}
