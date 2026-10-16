//! Connected LLM providers: built-in presets plus custom OpenAI-compatible endpoints.
//!
//! Custom providers live in the global `providers` settings block (never project settings,
//! so a repository cannot add an endpoint that would receive your keys). API keys live in
//! the credential store under the provider id.

use loop_ai::auth::{Credential, CredentialStore};
use loop_ai::providers::{
    custom_provider, provider_preset, CustomProviderConfig, ProviderPreset, PROVIDER_PRESETS,
};
use loop_ai::{Models, Provider};
use serde::{Deserialize, Serialize};

use super::tracing::validate_http_url;

/// A custom OpenAI-compatible provider added with `/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomProviderEntry {
    /// Provider id (slug of the name).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Base URL including the version path, e.g. `http://localhost:11434/v1`.
    pub base_url: String,
}

impl CustomProviderEntry {
    /// Build the provider. It works without a key; a saved key is still sent.
    pub fn provider(&self) -> Provider {
        custom_provider(CustomProviderConfig {
            id: self.id.clone(),
            name: Some(self.name.clone()),
            base_url: self.base_url.clone(),
            api_key_env: vec![],
            models: vec![],
            headers: None,
        })
    }
}

/// What `/login` collected.
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderLoginRequest {
    /// A built-in provider (Soket, OpenRouter, OpenAI).
    Preset {
        /// Preset id.
        id: String,
        /// API key.
        api_key: String,
    },
    /// Any OpenAI-compatible endpoint.
    Custom {
        /// Display name (the id is derived from it).
        name: String,
        /// Base URL including the version path.
        base_url: String,
        /// API key; `None` for keyless local servers.
        api_key: Option<String>,
    },
    /// A web-search backend (`tavily`, `brave`) for the `web_search` tool.
    WebSearch {
        /// Backend id.
        id: String,
        /// API key.
        api_key: String,
    },
}

impl std::fmt::Debug for ProviderLoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preset { id, .. } => f
                .debug_struct("Preset")
                .field("id", id)
                .finish_non_exhaustive(),
            Self::Custom { name, base_url, .. } => f
                .debug_struct("Custom")
                .field("name", name)
                .field("base_url", base_url)
                .finish_non_exhaustive(),
            Self::WebSearch { id, .. } => f
                .debug_struct("WebSearch")
                .field("id", id)
                .finish_non_exhaustive(),
        }
    }
}

impl ProviderLoginRequest {
    /// Provider id this request configures.
    pub fn provider_id(&self) -> String {
        match self {
            Self::Preset { id, .. } | Self::WebSearch { id, .. } => id.trim().to_ascii_lowercase(),
            Self::Custom { name, .. } => slugify(name),
        }
    }

    /// Display name.
    pub fn display_name(&self) -> String {
        match self {
            Self::Preset { id, .. } => {
                provider_preset(id).map_or_else(|| id.clone(), |p| p.name.into())
            }
            Self::Custom { name, .. } => name.trim().to_string(),
            Self::WebSearch { id, .. } => web_search_display_name(id),
        }
    }

    /// The key to store, if any (blank means none).
    pub fn api_key(&self) -> Option<&str> {
        let key = match self {
            Self::Preset { api_key, .. } => Some(api_key.as_str()),
            Self::Custom { api_key, .. } => api_key.as_deref(),
            Self::WebSearch { api_key, .. } => Some(api_key.as_str()),
        };
        key.map(str::trim).filter(|k| !k.is_empty())
    }

    /// The preset this request targets.
    pub fn preset(&self) -> Option<&'static ProviderPreset> {
        match self {
            Self::Preset { id, .. } => provider_preset(id),
            Self::Custom { .. } | Self::WebSearch { .. } => None,
        }
    }

    /// The settings entry to save (custom providers only).
    pub fn custom_entry(&self) -> Option<CustomProviderEntry> {
        match self {
            Self::Custom { base_url, .. } => Some(CustomProviderEntry {
                id: self.provider_id(),
                name: self.display_name(),
                base_url: base_url.trim().trim_end_matches('/').to_string(),
            }),
            Self::Preset { .. } | Self::WebSearch { .. } => None,
        }
    }

    /// Check the request before anything is saved.
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Preset { id, .. } => {
                let preset = provider_preset(id)
                    .ok_or_else(|| anyhow::anyhow!("unknown provider `{id}`"))?;
                if self.api_key().is_none() {
                    anyhow::bail!("{} needs an API key", preset.name);
                }
            }
            Self::Custom { name, base_url, .. } => {
                let id = self.provider_id();
                if name.trim().is_empty() || id.is_empty() {
                    anyhow::bail!("provider name must contain letters or digits");
                }
                if provider_preset(&id).is_some() {
                    anyhow::bail!("`{id}` is a built-in provider; pick another name");
                }
                validate_http_url("Base URL", base_url)?;
            }
            Self::WebSearch { id, .. } => {
                if !is_web_search_provider(&self.provider_id()) {
                    anyhow::bail!("unknown web-search provider `{id}`");
                }
                if self.api_key().is_none() {
                    anyhow::bail!("{} needs an API key", self.display_name());
                }
            }
        }
        Ok(())
    }
}

/// Whether `id` names a built-in web-search backend (see loop-agent's
/// `WEB_SEARCH_BACKENDS` registry — the single source of truth).
pub fn is_web_search_provider(id: &str) -> bool {
    loop_agent::harness::web_search_backend(id).is_some()
}

/// Display name for a web-search backend id.
pub fn web_search_display_name(id: &str) -> String {
    loop_agent::harness::web_search_backend(id)
        .map(|info| info.display_name.to_string())
        .unwrap_or_else(|| id.trim().to_string())
}

/// `"My Gateway!"` → `"my-gateway"`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// Register every preset and saved custom provider.
pub fn register_providers(models: &Models, custom: &[CustomProviderEntry]) {
    for preset in PROVIDER_PRESETS {
        models.set_provider(preset.provider());
    }
    for entry in custom {
        models.set_provider(entry.provider());
    }
}

/// Providers the user can chat with now: presets with a key (env or saved) and every
/// custom provider. Presets first, in `/login` order.
pub fn connected_providers(
    store: &dyn CredentialStore,
    custom: &[CustomProviderEntry],
    env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut ids: Vec<String> = PROVIDER_PRESETS
        .iter()
        .filter(|p| {
            !store.is_logged_out(p.id)
                && (store.get(p.id).is_some()
                    || p.api_key_envs
                        .iter()
                        .any(|e| env(e).is_some_and(|v| !v.trim().is_empty())))
        })
        .map(|p| p.id.to_string())
        .collect();
    ids.extend(
        custom
            .iter()
            .filter(|c| !store.is_logged_out(&c.id))
            .map(|c| c.id.clone()),
    );
    ids
}

/// Order for the `/model` picker: Soket first, other providers alphabetically by id,
/// models by id within a provider.
pub fn sort_models_for_picker(mut models: Vec<loop_ai::Model>) -> Vec<loop_ai::Model> {
    use loop_ai::providers::SOKET_PROVIDER_ID;
    models.sort_by(|a, b| {
        (a.provider != SOKET_PROVIDER_ID, &a.provider, &a.id).cmp(&(
            b.provider != SOKET_PROVIDER_ID,
            &b.provider,
            &b.id,
        ))
    });
    models
}

/// Save (or clear) the key for `id`; returns what was there before, for rollback.
pub fn replace_api_key(
    store: &dyn CredentialStore,
    id: &str,
    key: Option<&str>,
) -> Option<Credential> {
    let previous = store.get(id);
    restore_api_key(store, id, key.map(Credential::api_key));
    previous
}

/// Put back a credential captured by [`replace_api_key`].
pub fn restore_api_key(store: &dyn CredentialStore, id: &str, credential: Option<Credential>) {
    match credential {
        Some(credential) => store.set(id, credential),
        None => store.remove(id),
    }
}

/// Insert or replace a custom provider entry by id.
pub fn upsert_custom_provider(entries: &mut Vec<CustomProviderEntry>, entry: CustomProviderEntry) {
    match entries.iter_mut().find(|e| e.id == entry.id) {
        Some(existing) => *existing = entry,
        None => entries.push(entry),
    }
}

#[cfg(test)]
mod tests {
    use loop_ai::auth::InMemoryCredentialStore;

    use super::*;

    fn custom(name: &str, base_url: &str, key: Option<&str>) -> ProviderLoginRequest {
        ProviderLoginRequest::Custom {
            name: name.into(),
            base_url: base_url.into(),
            api_key: key.map(Into::into),
        }
    }

    fn preset(id: &str, key: &str) -> ProviderLoginRequest {
        ProviderLoginRequest::Preset {
            id: id.into(),
            api_key: key.into(),
        }
    }

    #[test]
    fn slugify_names() {
        assert_eq!(slugify("My Gateway!"), "my-gateway");
        assert_eq!(slugify("  LM  Studio  "), "lm-studio");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn preset_requests_need_a_known_id_and_a_key() {
        assert!(preset("openrouter", "sk-or-1").validate().is_ok());
        assert_eq!(preset("OpenRouter", "k").provider_id(), "openrouter");
        assert_eq!(preset("openrouter", "k").display_name(), "OpenRouter");
        assert!(preset("openrouter", "  ").validate().is_err());
        assert!(preset("groq", "k").validate().is_err());
    }

    #[test]
    fn custom_requests_validate_name_and_url_and_allow_no_key() {
        let ok = custom("LM Studio", "http://localhost:1234/v1/", None);
        assert!(ok.validate().is_ok());
        assert_eq!(ok.api_key(), None);
        assert_eq!(
            ok.custom_entry().unwrap(),
            CustomProviderEntry {
                id: "lm-studio".into(),
                name: "LM Studio".into(),
                base_url: "http://localhost:1234/v1".into(),
            }
        );
        assert!(custom("???", "http://x", None).validate().is_err());
        assert!(custom("Gateway", "localhost:1234", None)
            .validate()
            .is_err());
        let err = custom("OpenAI", "https://proxy.example/v1", None)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("built-in"));
    }

    #[test]
    fn debug_hides_keys() {
        let text = format!(
            "{:?} {:?}",
            preset("openrouter", "sk-or-secret"),
            custom("G", "http://x", Some("tok-secret"))
        );
        assert!(!text.contains("secret"));
    }

    #[test]
    fn connected_providers_lists_keyed_presets_then_customs() {
        let store = InMemoryCredentialStore::default();
        let customs = vec![CustomProviderEntry {
            id: "lm-studio".into(),
            name: "LM Studio".into(),
            base_url: "http://localhost:1234/v1".into(),
        }];
        assert_eq!(
            connected_providers(&store, &customs, |_| None),
            ["lm-studio"]
        );

        store.set("openrouter", Credential::api_key("k"));
        let env = |k: &str| (k == "SOKET_API_KEY").then(|| "s".to_string());
        assert_eq!(
            connected_providers(&store, &customs, env),
            ["soket", "openrouter", "lm-studio"]
        );
    }

    #[test]
    fn replace_and_restore_api_key_round_trip() {
        let store = InMemoryCredentialStore::default();
        store.set("openai", Credential::api_key("old"));
        let previous = replace_api_key(&store, "openai", Some("new"));
        assert!(matches!(store.get("openai"), Some(Credential::ApiKey { key }) if key == "new"));
        restore_api_key(&store, "openai", previous);
        assert!(matches!(store.get("openai"), Some(Credential::ApiKey { key }) if key == "old"));
        let previous = replace_api_key(&store, "fresh", None);
        assert!(previous.is_none() && store.get("fresh").is_none());
    }

    #[test]
    fn picker_order_puts_soket_first_then_providers_alphabetically() {
        let model = |provider: &str, id: &str| loop_ai::Model {
            provider: provider.into(),
            id: id.into(),
            ..loop_ai::providers::soket_seed_models()[0].clone()
        };
        let sorted = sort_models_for_picker(vec![
            model("openrouter", "z-model"),
            model("lm-studio", "local"),
            model("soket", "qwen-b"),
            model("openrouter", "anthropic/claude"),
            model("soket", "qwen-a"),
        ]);
        let order: Vec<_> = sorted
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(
            order,
            [
                "soket/qwen-a",
                "soket/qwen-b",
                "lm-studio/local",
                "openrouter/anthropic/claude",
                "openrouter/z-model",
            ]
        );
    }

    #[test]
    fn upsert_replaces_by_id() {
        let mut entries = vec![];
        let entry = |url: &str| CustomProviderEntry {
            id: "g".into(),
            name: "G".into(),
            base_url: url.into(),
        };
        upsert_custom_provider(&mut entries, entry("http://a"));
        upsert_custom_provider(&mut entries, entry("http://b"));
        assert_eq!(entries, [entry("http://b")]);
    }

    #[test]
    fn register_providers_adds_presets_and_customs() {
        let models = Models::new();
        register_providers(
            &models,
            &[CustomProviderEntry {
                id: "g".into(),
                name: "G".into(),
                base_url: "http://localhost:1/v1".into(),
            }],
        );
        for id in ["soket", "openrouter", "openai", "g"] {
            assert!(models.get_provider(id).is_some(), "{id}");
        }
    }
}
