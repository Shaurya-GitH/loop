//! `/login` wizard: pick an API-key provider and enter its key, or pick a
//! subscription (such as ChatGPT Plus/Pro) and sign in to it.
//!
//! Pure state machine; the app acts on the resulting [`LoginChoice`].

use loop_agent::harness::{web_search_backend, WebSearchBackendInfo, WEB_SEARCH_BACKENDS};
use loop_ai::providers::{provider_preset, ProviderPreset, PROVIDER_PRESETS};
use loop_app_core::config::providers::slugify;
use loop_app_core::config::{validate_http_url, ProviderLoginRequest};

use crate::wizard::{wrap_selection, WizardView};

/// `/login custom` and the last picker row.
pub const CUSTOM_PROVIDER: &str = "custom";
const CUSTOM_LABEL: &str = "Custom";
const CUSTOM_DESCRIPTION: &str =
    "Any OpenAI-compatible API · Together, Groq, DeepSeek, vLLM, Ollama, LM Studio";

/// Web search backends that get their own `/login` row. Soket search uses
/// the Soket model key, so it has no row of its own.
fn login_web_backends() -> impl Iterator<Item = &'static WebSearchBackendInfo> {
    WEB_SEARCH_BACKENDS
        .iter()
        .filter(|info| info.id != loop_ai::providers::SOKET_PROVIDER_ID)
}

/// A web search backend `/login <id>` can set up.
fn login_web_backend(id: &str) -> Option<&'static WebSearchBackendInfo> {
    web_search_backend(id).filter(|info| info.id != loop_ai::providers::SOKET_PROVIDER_ID)
}

/// Picker-row copy for a web-search backend. Ids and names come from
/// loop-agent's `WEB_SEARCH_BACKENDS` registry; only presentation lives here.
fn backend_description(info: &WebSearchBackendInfo) -> &'static str {
    match info.id {
        "brave" => "Web search for the web_search tool · brave.com/search-api",
        "tinyfish" => "Web search for the web_search tool · tinyfish.ai",
        _ => "Web search for the web_search tool · free tier available",
    }
}

/// Where to get an API key for a backend (shown in the key prompt).
fn backend_key_url(info: &WebSearchBackendInfo) -> &'static str {
    match info.id {
        "tavily" => "https://app.tavily.com/home",
        "brave" => "https://api-dashboard.search.brave.com/",
        _ => "",
    }
}

/// Which provider the key is for.
#[derive(Debug, Clone)]
pub enum LoginTarget {
    /// A built-in provider.
    Preset(&'static ProviderPreset),
    /// A web-search backend.
    WebSearch(&'static WebSearchBackendInfo),
    /// A custom OpenAI-compatible endpoint.
    Custom {
        /// Display name.
        name: String,
        /// Base URL.
        base_url: String,
    },
}

impl PartialEq for LoginTarget {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Preset(a), Self::Preset(b)) => a.id == b.id,
            (Self::WebSearch(a), Self::WebSearch(b)) => a.id == b.id,
            (
                Self::Custom { name, base_url },
                Self::Custom {
                    name: other_name,
                    base_url: other_url,
                },
            ) => name == other_name && base_url == other_url,
            _ => false,
        }
    }
}

impl Eq for LoginTarget {}

/// A subscription offered in `/login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// Subscription id (the provider id in `provider/model`).
    pub id: String,
    /// Row label, such as "ChatGPT Plus/Pro (Codex subscription)".
    pub label: String,
    /// Already signed in.
    pub configured: bool,
}

/// What the finished wizard asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginChoice {
    /// Connect an API-key provider.
    Provider(ProviderLoginRequest),
    /// Sign in to a subscription.
    Subscription(AgentRow),
}

/// Current step of the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSetup {
    /// Choose a provider, a subscription or Custom.
    Choose {
        /// Highlighted row.
        selected: usize,
        /// Subscriptions listed after the API-key presets.
        agents: Vec<AgentRow>,
        /// Preset ids that already have a key.
        connected: Vec<String>,
    },
    /// Custom provider name.
    CustomName,
    /// Custom provider base URL.
    CustomBaseUrl {
        /// Entered name.
        name: String,
    },
    /// API key (masked).
    ApiKey {
        /// Provider the key is for.
        target: LoginTarget,
    },
}

/// Result of submitting an answer to the login wizard.
pub type Transition = crate::wizard::Transition<ProviderSetup, LoginChoice>;

impl ProviderSetup {
    /// Start the wizard; `provider` (`/login openrouter`, `/login tavily`,
    /// `/login custom`) skips the picker, and a subscription id
    /// (`/login chatgpt`) highlights it. `connected` lists preset and web
    /// search ids that already have a key.
    pub fn start(
        provider: Option<&str>,
        agents: Vec<AgentRow>,
        connected: Vec<String>,
    ) -> Result<Self, String> {
        match provider.map(str::trim).filter(|p| !p.is_empty()) {
            None => Ok(Self::Choose {
                selected: 0,
                agents,
                connected,
            }),
            Some(p) if p.eq_ignore_ascii_case(CUSTOM_PROVIDER) => Ok(Self::CustomName),
            Some(p) => {
                if let Some(preset) = provider_preset(p) {
                    return Ok(Self::ApiKey {
                        target: LoginTarget::Preset(preset),
                    });
                }
                if let Some(info) = login_web_backend(p) {
                    return Ok(Self::ApiKey {
                        target: LoginTarget::WebSearch(info),
                    });
                }
                match agents.iter().position(|a| a.id.eq_ignore_ascii_case(p)) {
                    Some(i) => Ok(Self::Choose {
                        selected: PROVIDER_PRESETS.len() + i,
                        agents,
                        connected,
                    }),
                    None => Err(format!("unknown provider `{p}`. {}", login_usage(&agents))),
                }
            }
        }
    }

    /// Re-ask for the key after `request` was rejected (e.g. HTTP 401).
    pub fn retry_key(request: &ProviderLoginRequest) -> Self {
        let target = match request {
            ProviderLoginRequest::Preset { id, .. } => match provider_preset(id) {
                Some(preset) => LoginTarget::Preset(preset),
                None => {
                    return Self::Choose {
                        selected: 0,
                        agents: Vec::new(),
                        connected: Vec::new(),
                    }
                }
            },
            ProviderLoginRequest::WebSearch { id, .. } => match web_search_backend(id) {
                Some(info) => LoginTarget::WebSearch(info),
                None => {
                    return Self::Choose {
                        selected: 0,
                        agents: Vec::new(),
                        connected: Vec::new(),
                    }
                }
            },
            ProviderLoginRequest::Custom { name, base_url, .. } => LoginTarget::Custom {
                name: name.clone(),
                base_url: base_url.clone(),
            },
        };
        Self::ApiKey { target }
    }

    fn row_count(agents: &[AgentRow]) -> usize {
        PROVIDER_PRESETS.len() + agents.len() + login_web_backends().count() + 1
    }

    /// Submit the input line at the current step.
    pub fn submit(self, input: &str) -> Transition {
        let value = input.trim().to_string();
        match self {
            Self::Choose {
                selected, agents, ..
            } => {
                let step = match PROVIDER_PRESETS.get(selected) {
                    Some(preset) => Self::ApiKey {
                        target: LoginTarget::Preset(preset),
                    },
                    None => match agents.get(selected - PROVIDER_PRESETS.len()) {
                        Some(agent) => {
                            return Transition::Done(LoginChoice::Subscription(agent.clone()))
                        }
                        None => match login_web_backends()
                            .nth(selected - PROVIDER_PRESETS.len() - agents.len())
                        {
                            Some(info) => Self::ApiKey {
                                target: LoginTarget::WebSearch(info),
                            },
                            None => Self::CustomName,
                        },
                    },
                };
                Transition::Next {
                    step,
                    prefill: String::new(),
                }
            }
            Self::CustomName => {
                let id = slugify(&value);
                if id.is_empty() {
                    Self::CustomName.retry("name must contain letters or digits")
                } else if provider_preset(&id).is_some() {
                    Self::CustomName.retry(&format!(
                        "`{id}` is a built-in provider — pick it from /login instead"
                    ))
                } else {
                    Transition::Next {
                        step: Self::CustomBaseUrl { name: value },
                        prefill: String::new(),
                    }
                }
            }
            Self::CustomBaseUrl { name } => match validate_http_url("Base URL", &value) {
                Ok(()) => Transition::Next {
                    step: Self::ApiKey {
                        target: LoginTarget::Custom {
                            name,
                            base_url: value,
                        },
                    },
                    prefill: String::new(),
                },
                Err(err) => Self::CustomBaseUrl { name }.retry(&err.to_string()),
            },
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } if value.is_empty() => Self::ApiKey {
                target: LoginTarget::Preset(preset),
            }
            .retry(&format!("{} needs an API key", preset.name)),
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => Transition::Done(LoginChoice::Provider(ProviderLoginRequest::Preset {
                id: preset.id.into(),
                api_key: value,
            })),
            Self::ApiKey {
                target: LoginTarget::WebSearch(info),
            } if value.is_empty() => Self::ApiKey {
                target: LoginTarget::WebSearch(info),
            }
            .retry(&format!("{} needs an API key", info.display_name)),
            Self::ApiKey {
                target: LoginTarget::WebSearch(info),
            } => Transition::Done(LoginChoice::Provider(ProviderLoginRequest::WebSearch {
                id: info.id.into(),
                api_key: value,
            })),
            Self::ApiKey {
                target: LoginTarget::Custom { name, base_url },
            } => Transition::Done(LoginChoice::Provider(ProviderLoginRequest::Custom {
                name,
                base_url,
                api_key: (!value.is_empty()).then_some(value),
            })),
        }
    }

    fn retry(self, error: &str) -> Transition {
        Transition::Retry {
            step: self,
            error: error.into(),
        }
    }
}

/// Usage line for `/login`.
pub fn login_usage(agents: &[AgentRow]) -> String {
    let ids: Vec<&str> = PROVIDER_PRESETS
        .iter()
        .map(|p| p.id)
        .chain(agents.iter().map(|a| a.id.as_str()))
        .chain(login_web_backends().map(|info| info.id))
        .chain([CUSTOM_PROVIDER])
        .collect();
    format!("Usage: /login [{}]", ids.join("|"))
}

impl WizardView for ProviderSetup {
    fn title(&self) -> String {
        match self {
            Self::Choose { .. } => "Connect a model provider, subscription or web search".into(),
            Self::CustomName => "Connect a custom provider · step 1 of 3".into(),
            Self::CustomBaseUrl { .. } => "Connect a custom provider · step 2 of 3".into(),
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => format!("Connect {}", preset.name),
            Self::ApiKey {
                target: LoginTarget::WebSearch(info),
            } => format!("Connect {}", info.display_name),
            Self::ApiKey {
                target: LoginTarget::Custom { name, .. },
            } => format!("Connect {name} · step 3 of 3"),
        }
    }

    fn instructions(&self) -> String {
        match self {
            Self::Choose { .. } => {
                "Choose where Loop sends model requests (or add web search)".into()
            }
            Self::CustomName => "A name for this provider, e.g. Together, Groq or LM Studio".into(),
            Self::CustomBaseUrl { name } => {
                format!("Base URL of {name}'s OpenAI-compatible API, including the version path")
            }
            Self::ApiKey {
                target: LoginTarget::Preset(preset),
            } => format!(
                "Paste your {} API key ({}) — input stays hidden · {} {}",
                preset.name, preset.key_hint, preset.key_cta, preset.key_url
            ),
            Self::ApiKey {
                target: LoginTarget::WebSearch(info),
            } => format!(
                "Paste your {} API key — input stays hidden · get one at {} · powers the web_search tool",
                info.display_name,
                backend_key_url(info)
            ),
            Self::ApiKey {
                target: LoginTarget::Custom { name, .. },
            } => {
                format!("API key for {name} — input stays hidden · enter to skip for local servers")
            }
        }
    }

    fn placeholder(&self) -> &'static str {
        match self {
            Self::Choose { .. } => " ↑↓ to choose, enter to continue",
            Self::CustomName => " e.g. Together",
            Self::CustomBaseUrl { .. } => {
                " https://api.together.xyz/v1 or http://localhost:11434/v1"
            }
            Self::ApiKey {
                target: LoginTarget::Custom { .. },
            } => " API key (optional)",
            Self::ApiKey { .. } => " paste your API key",
        }
    }

    fn options(&self) -> Option<(Vec<(String, String)>, usize)> {
        let state = |configured: bool| {
            if configured {
                "configured"
            } else {
                "not configured"
            }
        };
        match self {
            Self::Choose {
                selected,
                agents,
                connected,
            } => Some((
                PROVIDER_PRESETS
                    .iter()
                    .map(|p| {
                        let configured = connected.iter().any(|c| c == p.id);
                        (
                            format!("{} · api key", p.name),
                            format!("{} · {}", state(configured), p.description),
                        )
                    })
                    .chain(agents.iter().map(|a| {
                        (
                            format!("{} · subscription", a.label),
                            state(a.configured).to_owned(),
                        )
                    }))
                    .chain(login_web_backends().map(|info| {
                        let configured = connected.iter().any(|c| c == info.id);
                        (
                            format!("{} · web search", info.display_name),
                            format!("{} · {}", state(configured), backend_description(info)),
                        )
                    }))
                    .chain([(CUSTOM_LABEL.to_owned(), CUSTOM_DESCRIPTION.to_owned())])
                    .collect(),
                *selected,
            )),
            _ => None,
        }
    }

    fn masked(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
    }

    /// No env-var tip on login; preset key envs are still read at startup.
    fn env_hint(&self) -> Option<String> {
        None
    }

    fn move_selection(&mut self, delta: isize) {
        if let Self::Choose {
            selected, agents, ..
        } = self
        {
            *selected = wrap_selection(*selected, delta, Self::row_count(agents));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(t: Transition) -> ProviderSetup {
        match t {
            Transition::Next { step, .. } => step,
            other => panic!("expected Next, got {other:?}"),
        }
    }

    fn retry_error(t: Transition) -> String {
        match t {
            Transition::Retry { error, .. } => error,
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn picker_lists_presets_then_web_search_then_custom() {
        let step = ProviderSetup::start(None, Vec::new(), Vec::new()).unwrap();
        let (rows, selected) = step.options().unwrap();
        let labels: Vec<_> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Soket · api key",
                "OpenRouter · api key",
                "OpenAI · api key",
                "Tavily · web search",
                "TinyFish · web search",
                "Brave Search · web search",
                "Custom"
            ]
        );
        assert_eq!(selected, 0);
        assert!(!step.masked());
    }

    #[test]
    fn selection_wraps_over_all_rows() {
        let mut step = ProviderSetup::Choose {
            selected: 0,
            agents: Vec::new(),
            connected: Vec::new(),
        };
        step.move_selection(-1);
        assert_eq!(
            step,
            ProviderSetup::Choose {
                selected: 6,
                agents: Vec::new(),
                connected: Vec::new(),
            }
        );
    }

    #[test]
    fn tavily_flow_from_picker_and_argument() {
        // From the picker (row 3).
        let mut step = ProviderSetup::start(None, Vec::new(), Vec::new()).unwrap();
        step.move_selection(3);
        let key = next(step.submit(""));
        assert!(key.masked());
        assert_eq!(key.title(), "Connect Tavily");
        assert!(key.instructions().contains("web_search"));
        assert!(retry_error(key.clone().submit("  ")).contains("needs an API key"));
        assert_eq!(
            key.submit(" tvly-1 "),
            Transition::Done(LoginChoice::Provider(ProviderLoginRequest::WebSearch {
                id: "tavily".into(),
                api_key: "tvly-1".into(),
            }))
        );

        // Direct: /login tavily skips the picker.
        let direct = ProviderSetup::start(Some("Tavily"), Vec::new(), Vec::new()).unwrap();
        assert_eq!(direct.title(), "Connect Tavily");
    }

    #[test]
    fn openrouter_flow() {
        let mut step = ProviderSetup::start(None, Vec::new(), Vec::new()).unwrap();
        step.move_selection(1);
        let key = next(step.submit(""));
        assert!(key.masked());
        assert_eq!(key.title(), "Connect OpenRouter");
        assert_eq!(key.env_hint(), None);
        assert!(key.instructions().contains("openrouter.ai/keys"));
        assert!(retry_error(key.clone().submit("  ")).contains("needs an API key"));
        assert_eq!(
            key.submit(" sk-or-1 "),
            Transition::Done(LoginChoice::Provider(ProviderLoginRequest::Preset {
                id: "openrouter".into(),
                api_key: "sk-or-1".into(),
            }))
        );
    }

    #[test]
    fn custom_flow_allows_empty_key_and_validates() {
        let step = ProviderSetup::start(Some("custom"), Vec::new(), Vec::new()).unwrap();
        assert_eq!(step, ProviderSetup::CustomName);
        assert!(retry_error(ProviderSetup::CustomName.submit("!!")).contains("letters"));
        assert!(retry_error(ProviderSetup::CustomName.submit("OpenAI")).contains("built-in"));
        let url = next(step.submit("LM Studio"));
        assert!(retry_error(url.clone().submit("localhost:1234")).contains("http://"));
        let key = next(url.submit("http://localhost:1234/v1"));
        assert!(key.masked());
        assert_eq!(key.title(), "Connect LM Studio · step 3 of 3");
        assert_eq!(
            key.submit(""),
            Transition::Done(LoginChoice::Provider(ProviderLoginRequest::Custom {
                name: "LM Studio".into(),
                base_url: "http://localhost:1234/v1".into(),
                api_key: None,
            }))
        );
    }

    #[test]
    fn start_with_argument_skips_the_picker() {
        let step = ProviderSetup::start(Some("OpenAI"), Vec::new(), Vec::new()).unwrap();
        assert_eq!(step.title(), "Connect OpenAI");
        let err = ProviderSetup::start(Some("groq"), Vec::new(), Vec::new()).unwrap_err();
        assert!(
            err.contains("Usage: /login [soket|openrouter|openai|tavily|tinyfish|brave|custom]"),
            "{err}"
        );
    }

    #[test]
    fn retry_key_returns_to_the_key_step() {
        let preset = ProviderLoginRequest::Preset {
            id: "openai".into(),
            api_key: "bad".into(),
        };
        assert_eq!(ProviderSetup::retry_key(&preset).title(), "Connect OpenAI");
        let custom = ProviderLoginRequest::Custom {
            name: "G".into(),
            base_url: "http://g/v1".into(),
            api_key: None,
        };
        assert!(matches!(
            ProviderSetup::retry_key(&custom),
            ProviderSetup::ApiKey {
                target: LoginTarget::Custom { .. }
            }
        ));
        let web = ProviderLoginRequest::WebSearch {
            id: "brave".into(),
            api_key: "bad".into(),
        };
        assert_eq!(
            ProviderSetup::retry_key(&web).title(),
            "Connect Brave Search"
        );
    }

    fn chatgpt(configured: bool) -> AgentRow {
        AgentRow {
            id: "chatgpt".into(),
            label: "ChatGPT Plus/Pro (Codex subscription)".into(),
            configured,
        }
    }

    #[test]
    fn subscriptions_are_rows_with_their_state_and_sign_in_directly() {
        let step = ProviderSetup::start(None, vec![chatgpt(false)], vec!["openai".into()]).unwrap();
        let (rows, _) = step.options().unwrap();
        assert_eq!(rows[2].0, "OpenAI · api key");
        assert!(rows[2].1.starts_with("configured"));
        assert_eq!(
            rows[3].0,
            "ChatGPT Plus/Pro (Codex subscription) · subscription"
        );
        assert_eq!(rows[3].1, "not configured");
        assert_eq!(rows[4].0, "Tavily · web search");
        assert_eq!(rows.last().unwrap().0, "Custom");

        let step = ProviderSetup::start(Some("chatgpt"), vec![chatgpt(false)], Vec::new()).unwrap();
        assert_eq!(
            step.submit(""),
            Transition::Done(LoginChoice::Subscription(chatgpt(false)))
        );
        let err = ProviderSetup::start(Some("nope"), vec![chatgpt(true)], Vec::new()).unwrap_err();
        assert!(err.contains("openai|chatgpt|tavily"), "{err}");
    }

    #[test]
    fn soket_is_one_row_and_web_rows_show_their_state() {
        let step = ProviderSetup::start(None, Vec::new(), vec!["tavily".into()]).unwrap();
        let (rows, _) = step.options().unwrap();
        assert_eq!(rows.iter().filter(|r| r.0.starts_with("Soket")).count(), 1);
        let tavily = rows.iter().find(|r| r.0 == "Tavily · web search").unwrap();
        assert!(tavily.1.starts_with("configured"));
        // `/login soket` is the model provider; its key also serves web search.
        let soket = ProviderSetup::start(Some("soket"), Vec::new(), Vec::new()).unwrap();
        assert_eq!(soket.title(), "Connect Soket");
    }
}
