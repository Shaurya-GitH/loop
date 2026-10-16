//! Which web tools a runtime registers, and with which keys.
//!
//! Web search and fetch are on by default: search uses `webSearch.provider`
//! (Soket unless set) and fetch goes through Soket when a Soket key exists,
//! directly otherwise. `webSearch.enabled: false`, `/websearch off` and
//! `--no-web` drop both tools.

use std::sync::Arc;

use loop_agent::harness::{
    create_web_fetch_tool, create_web_search_tool, web_search_backend, SoketWebApi, WebFetchConfig,
    WebFetchProvider, WebSearchConfig,
};
use loop_agent::types::AgentTool;
use loop_ai::providers::SOKET_PROVIDER_ID;
use loop_ai::CredentialStore;

use crate::config::{FileCredentialStore, Settings};

/// Everything needed to (re)build the web tools; cheap to clone into tasks.
#[derive(Clone)]
pub struct WebTools {
    /// `webSearch.enabled` and not started with `--no-web`.
    pub enabled: bool,
    /// `webFetch` settings.
    pub fetch: WebFetchConfig,
    /// `webSearch` settings (defaults when absent).
    pub search: WebSearchConfig,
    /// Where `/login` keys live (the Soket key included).
    pub credentials: Option<Arc<FileCredentialStore>>,
}

impl WebTools {
    /// The web tools `settings` asks for; `disabled_for_run` is `--no-web`.
    pub fn from_settings(
        settings: &Settings,
        credentials: Option<Arc<FileCredentialStore>>,
        disabled_for_run: bool,
    ) -> Self {
        let search = settings.web_search.clone().unwrap_or_default();
        Self {
            enabled: search.enabled && !disabled_for_run,
            fetch: settings.web_fetch.unwrap_or_default(),
            search,
            credentials,
        }
    }

    /// The stored API key for `id` (auth.json), if any.
    fn stored_key(&self, id: &str) -> Option<String> {
        self.credentials
            .as_ref()
            .and_then(|store| store.get(id))
            .and_then(|credential| match credential {
                loop_ai::Credential::ApiKey { key } => Some(key),
                _ => None,
            })
    }

    fn soket(&self) -> Option<SoketWebApi> {
        if self.is_logged_out(SOKET_PROVIDER_ID) {
            return None;
        }
        SoketWebApi::resolve(self.stored_key(SOKET_PROVIDER_ID).as_deref())
    }

    fn is_logged_out(&self, id: &str) -> bool {
        self.credentials
            .as_ref()
            .is_some_and(|store| store.is_logged_out(id))
    }

    /// Build the tools, with a note for each one that could not be built.
    pub fn build_with_notes(&self) -> (Vec<AgentTool>, Vec<String>) {
        let (mut tools, mut notes) = (Vec::new(), Vec::new());
        if !self.enabled {
            return (tools, notes);
        }
        let fetch = if self.fetch.provider == WebFetchProvider::Soket
            && self.is_logged_out(SOKET_PROVIDER_ID)
        {
            Err("Soket is logged out; run /login soket".into())
        } else {
            create_web_fetch_tool(self.fetch, self.soket())
        };
        match fetch {
            Ok(tool) => tools.push(tool),
            Err(e) => notes.push(format!("web_fetch unavailable: {e}")),
        }
        let provider = self.search.provider_id();
        let search = if self.is_logged_out(&provider) {
            Err(format!("{provider} is logged out; run /login {provider}"))
        } else {
            create_web_search_tool(self.search.clone(), self.stored_key(&provider).as_deref())
        };
        match search {
            Ok(tool) => tools.push(tool),
            Err(e) => notes.push(format!("web_search unavailable: {e}")),
        }
        for note in &notes {
            tracing::info!("{note}");
        }
        (tools, notes)
    }

    /// Build the tools (failures are logged, see [`Self::build_with_notes`]).
    pub fn build(&self) -> Vec<AgentTool> {
        self.build_with_notes().0
    }

    /// Which path `web_fetch` takes.
    fn fetch_route(&self) -> &'static str {
        match self.fetch.provider {
            WebFetchProvider::Direct => "direct",
            WebFetchProvider::Soket if self.soket().is_none() => "unavailable (Soket disconnected)",
            WebFetchProvider::Soket => "via Soket",
            WebFetchProvider::Auto if self.fetch.allow_private_addresses => "direct",
            WebFetchProvider::Auto if self.soket().is_some() => "via Soket",
            WebFetchProvider::Auto if self.is_logged_out(SOKET_PROVIDER_ID) => {
                "direct (Soket disconnected)"
            }
            WebFetchProvider::Auto => "direct (no Soket key)",
        }
    }

    /// One-line status for `/websearch`.
    pub fn status(&self) -> String {
        if !self.enabled {
            return "web tools off · /websearch on to enable".into();
        }
        let provider = self.search.provider_id();
        let name = web_search_backend(&provider).map_or(provider.as_str(), |b| b.display_name);
        let (tools, notes) = self.build_with_notes();
        let search_status = if tools.iter().any(|tool| tool.name == "web_search") {
            name.to_owned()
        } else {
            format!("{name} unavailable")
        };
        let mut line = format!(
            "web tools on · search: {search_status} · fetch: {}",
            self.fetch_route()
        );
        for note in notes {
            line.push_str(" · ");
            line.push_str(&note);
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(keys: &[(&str, &str)]) -> (tempfile::TempDir, Arc<FileCredentialStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::open(dir.path().join("auth.json")).unwrap();
        for (id, key) in keys {
            store.set(id, loop_ai::Credential::api_key(*key));
        }
        (dir, Arc::new(store))
    }

    fn names(tools: &[AgentTool]) -> Vec<&str> {
        tools.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn on_by_default_with_the_stored_soket_key() {
        let (_dir, store) = store_with(&[("soket", "sk-stored")]);
        let web = WebTools::from_settings(&Settings::default(), Some(store), false);
        assert!(web.enabled);
        assert_eq!(names(&web.build()), ["web_fetch", "web_search"]);
        assert!(web.status().contains("search: Soket · fetch: via Soket"));
    }

    #[test]
    fn switched_off_by_setting_or_run_flag() {
        let (_dir, store) = store_with(&[("soket", "sk-stored")]);
        let mut settings = Settings::default();
        let web = WebTools::from_settings(&settings, Some(Arc::clone(&store)), true);
        assert!(web.build().is_empty(), "--no-web registers nothing");
        assert!(web.status().contains("off"));

        settings.web_search = Some(WebSearchConfig {
            enabled: false,
            ..Default::default()
        });
        let web = WebTools::from_settings(&settings, Some(store), false);
        assert!(
            web.build().is_empty(),
            "webSearch.enabled=false registers nothing"
        );
    }

    #[test]
    fn other_backends_use_their_own_stored_key() {
        let (_dir, store) = store_with(&[("tavily", "tvly-stored")]);
        let settings = Settings {
            web_search: Some(WebSearchConfig {
                provider: Some("tavily".into()),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let web = WebTools::from_settings(&settings, Some(store), false);
        assert!(names(&web.build()).contains(&"web_search"));
        assert!(web.status().contains("search: Tavily"));
    }
}
