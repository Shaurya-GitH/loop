//! `/tracing setup` wizard: pick a backend, then answer one prompt per field.
//!
//! Pure state machine; the TUI renders [`TracingSetup`] and feeds it the input line.

use loop_app_core::config::tracing::{
    DEFAULT_LANGFUSE_HOST, DEFAULT_OTLP_ENDPOINT, ENV_LANGFUSE_HOST, ENV_LANGFUSE_PUBLIC_KEY,
    ENV_LANGFUSE_SECRET_KEY,
};
use loop_app_core::config::{
    validate_http_url, TracingBackend, TracingSettings, TracingSetupRequest,
};

use crate::wizard::{wrap_selection, WizardView};

/// Current step of the wizard.
#[derive(Clone, PartialEq, Eq)]
pub enum TracingSetup {
    /// Choose a backend from [`TracingBackend::ALL`].
    Choose {
        /// Highlighted row.
        selected: usize,
    },
    /// Langfuse base URL.
    LangfuseHost,
    /// Langfuse public key.
    LangfusePublicKey {
        /// Entered host.
        host: String,
    },
    /// Langfuse secret key (masked).
    LangfuseSecret {
        /// Entered host.
        host: String,
        /// Entered public key.
        public_key: String,
    },
    /// Collector URL.
    OtlpEndpoint,
    /// Optional `Authorization` header value (masked).
    OtlpAuth {
        /// Entered endpoint.
        endpoint: String,
    },
}

impl std::fmt::Debug for TracingSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only step names: earlier answers are not secret, but keep logs terse.
        f.write_str(match self {
            Self::Choose { .. } => "Choose",
            Self::LangfuseHost => "LangfuseHost",
            Self::LangfusePublicKey { .. } => "LangfusePublicKey",
            Self::LangfuseSecret { .. } => "LangfuseSecret",
            Self::OtlpEndpoint => "OtlpEndpoint",
            Self::OtlpAuth { .. } => "OtlpAuth",
        })
    }
}

/// Result of submitting an answer to the tracing wizard.
pub type Transition = crate::wizard::Transition<TracingSetup, TracingSetupRequest>;

impl TracingSetup {
    /// Start the wizard; `backend` skips the picker (`/tracing setup langfuse`).
    pub fn start(backend: Option<TracingBackend>, saved: &TracingSettings) -> (Self, String) {
        match backend {
            Some(backend) => Self::first_step(backend, saved),
            None => {
                let selected = TracingBackend::ALL
                    .iter()
                    .position(|b| *b == saved.backend)
                    .unwrap_or(0);
                (Self::Choose { selected }, String::new())
            }
        }
    }

    fn first_step(backend: TracingBackend, saved: &TracingSettings) -> (Self, String) {
        match backend {
            TracingBackend::Langfuse => (
                Self::LangfuseHost,
                saved
                    .langfuse_host
                    .clone()
                    .unwrap_or_else(|| DEFAULT_LANGFUSE_HOST.into()),
            ),
            TracingBackend::Otlp => (
                Self::OtlpEndpoint,
                saved
                    .otlp_endpoint
                    .clone()
                    .unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.into()),
            ),
        }
    }

    /// Submit the input line at the current step.
    pub fn submit(self, input: &str, saved: &TracingSettings) -> Transition {
        let value = input.trim().to_string();
        match self {
            Self::Choose { selected } => {
                let (step, prefill) = Self::first_step(TracingBackend::ALL[selected], saved);
                Transition::Next { step, prefill }
            }
            Self::LangfuseHost => match validate_http_url("Langfuse host", &value) {
                Ok(()) => Transition::Next {
                    step: Self::LangfusePublicKey { host: value },
                    prefill: saved.langfuse_public_key.clone().unwrap_or_default(),
                },
                Err(err) => Self::LangfuseHost.retry(err.to_string()),
            },
            Self::LangfusePublicKey { host } if value.is_empty() => {
                Self::LangfusePublicKey { host }.retry("public key is required".into())
            }
            Self::LangfusePublicKey { host } => Transition::Next {
                step: Self::LangfuseSecret {
                    host,
                    public_key: value,
                },
                prefill: String::new(),
            },
            Self::LangfuseSecret { host, public_key } if value.is_empty() => {
                Self::LangfuseSecret { host, public_key }.retry("secret key is required".into())
            }
            Self::LangfuseSecret { host, public_key } => {
                Transition::Done(TracingSetupRequest::Langfuse {
                    host,
                    public_key,
                    secret_key: value,
                })
            }
            Self::OtlpEndpoint => match validate_http_url("OTLP endpoint", &value) {
                Ok(()) => Transition::Next {
                    step: Self::OtlpAuth { endpoint: value },
                    prefill: String::new(),
                },
                Err(err) => Self::OtlpEndpoint.retry(err.to_string()),
            },
            Self::OtlpAuth { endpoint } => Transition::Done(TracingSetupRequest::Otlp {
                endpoint,
                authorization: (!value.is_empty()).then_some(value),
            }),
        }
    }

    fn retry(self, error: String) -> Transition {
        Transition::Retry { step: self, error }
    }
}

impl WizardView for TracingSetup {
    /// Whether the input line must be hidden.
    fn masked(&self) -> bool {
        matches!(self, Self::LangfuseSecret { .. } | Self::OtlpAuth { .. })
    }

    /// Heading of the setup box.
    fn title(&self) -> String {
        let (backend, step, of) = match self {
            Self::Choose { .. } => return "Set up tracing".into(),
            Self::LangfuseHost => ("Langfuse", 1, 3),
            Self::LangfusePublicKey { .. } => ("Langfuse", 2, 3),
            Self::LangfuseSecret { .. } => ("Langfuse", 3, 3),
            Self::OtlpEndpoint => ("OTLP endpoint", 1, 2),
            Self::OtlpAuth { .. } => ("OTLP endpoint", 2, 2),
        };
        format!("Set up tracing · {backend} · step {step} of {of}")
    }

    /// What to enter at this step.
    fn instructions(&self) -> String {
        match self {
            Self::Choose { .. } => "Choose where Loop sends traces".into(),
            Self::LangfuseHost => {
                "Langfuse URL — cloud (https://cloud.langfuse.com) or your self-hosted host".into()
            }
            Self::LangfusePublicKey { host } => format!("Public key (pk-lf-…) for {host}"),
            Self::LangfuseSecret { .. } => {
                "Secret key (sk-lf-…) — input stays hidden, saved to auth.json".into()
            }
            Self::OtlpEndpoint => {
                "Collector URL — /v1/traces is added if missing (OTLP over HTTP, protobuf)".into()
            }
            Self::OtlpAuth { .. } => {
                "Authorization header value, e.g. `Bearer <token>` — optional, enter to skip".into()
            }
        }
    }

    /// Placeholder shown while the input line is empty.
    fn placeholder(&self) -> &'static str {
        match self {
            Self::Choose { .. } => " ↑↓ to choose, enter to continue",
            Self::LangfuseHost => " https://cloud.langfuse.com",
            Self::LangfusePublicKey { .. } => " pk-lf-…",
            Self::LangfuseSecret { .. } => " sk-lf-…",
            Self::OtlpEndpoint => " http://localhost:4318",
            Self::OtlpAuth { .. } => " Bearer <token> (optional)",
        }
    }

    /// Picker rows `(label, description)` and the highlighted index, at the first step.
    fn options(&self) -> Option<(Vec<(String, String)>, usize)> {
        match self {
            Self::Choose { selected } => Some((
                TracingBackend::ALL
                    .iter()
                    .map(|b| (b.label().to_owned(), b.description().to_owned()))
                    .collect(),
                *selected,
            )),
            _ => None,
        }
    }

    /// Env var alternative, where one exists.
    fn env_hint(&self) -> Option<String> {
        match self {
            Self::LangfuseHost | Self::LangfusePublicKey { .. } | Self::LangfuseSecret { .. } => {
                Some(format!(
                    "{ENV_LANGFUSE_HOST}, {ENV_LANGFUSE_PUBLIC_KEY} and {ENV_LANGFUSE_SECRET_KEY}"
                ))
            }
            _ => None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if let Self::Choose { selected } = self {
            *selected = wrap_selection(*selected, delta, TracingBackend::ALL.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(t: Transition) -> (TracingSetup, String) {
        match t {
            Transition::Next { step, prefill } => (step, prefill),
            other => panic!("expected Next, got {other:?}"),
        }
    }

    #[test]
    fn picker_lists_backends_and_preselects_saved_one() {
        let (step, prefill) = TracingSetup::start(None, &TracingSettings::default());
        let (rows, selected) = step.options().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["Langfuse", "OTLP endpoint"]
        );
        assert_eq!(selected, 0);
        assert!(prefill.is_empty());

        let saved = TracingSettings {
            backend: TracingBackend::Otlp,
            ..Default::default()
        };
        let (step, _) = TracingSetup::start(None, &saved);
        assert_eq!(step, TracingSetup::Choose { selected: 1 });
    }

    #[test]
    fn selection_wraps() {
        let mut step = TracingSetup::Choose { selected: 0 };
        step.move_selection(-1);
        assert_eq!(step, TracingSetup::Choose { selected: 1 });
        step.move_selection(1);
        assert_eq!(step, TracingSetup::Choose { selected: 0 });
    }

    #[test]
    fn langfuse_flow_collects_three_fields_with_prefills() {
        let saved = TracingSettings::default();
        let (step, _) = TracingSetup::start(None, &saved);
        let (step, prefill) = next(step.submit("", &saved));
        assert_eq!(step, TracingSetup::LangfuseHost);
        assert_eq!(prefill, "https://cloud.langfuse.com");
        assert!(!step.masked());
        assert_eq!(step.title(), "Set up tracing · Langfuse · step 1 of 3");

        let (step, _) = next(step.submit(" https://fuse.example ", &saved));
        assert_eq!(
            step,
            TracingSetup::LangfusePublicKey {
                host: "https://fuse.example".into()
            }
        );
        let (step, prefill) = next(step.submit("pk-lf-1", &saved));
        assert!(step.masked());
        assert!(prefill.is_empty());
        assert_eq!(
            step.submit("sk-lf-2", &saved),
            Transition::Done(TracingSetupRequest::Langfuse {
                host: "https://fuse.example".into(),
                public_key: "pk-lf-1".into(),
                secret_key: "sk-lf-2".into(),
            })
        );
    }

    #[test]
    fn saved_langfuse_values_prefill_host_and_public_key() {
        let saved = TracingSettings {
            langfuse_host: Some("https://fuse.example".into()),
            langfuse_public_key: Some("pk-saved".into()),
            ..Default::default()
        };
        let (step, prefill) = TracingSetup::start(Some(TracingBackend::Langfuse), &saved);
        assert_eq!(prefill, "https://fuse.example");
        let (_, prefill) = next(step.submit(&prefill, &saved));
        assert_eq!(prefill, "pk-saved");
    }

    #[test]
    fn invalid_or_empty_answers_retry_the_same_step() {
        let saved = TracingSettings::default();
        match TracingSetup::LangfuseHost.submit("fuse.example", &saved) {
            Transition::Retry { step, error } => {
                assert_eq!(step, TracingSetup::LangfuseHost);
                assert!(error.contains("http://"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        let pk = TracingSetup::LangfusePublicKey { host: "h".into() };
        assert!(matches!(pk.submit("  ", &saved), Transition::Retry { .. }));
        let sk = TracingSetup::LangfuseSecret {
            host: "h".into(),
            public_key: "p".into(),
        };
        assert!(matches!(sk.submit("", &saved), Transition::Retry { .. }));
        assert!(matches!(
            TracingSetup::OtlpEndpoint.submit("localhost:4318", &saved),
            Transition::Retry { .. }
        ));
    }

    #[test]
    fn otlp_flow_with_and_without_authorization() {
        let saved = TracingSettings::default();
        let (step, prefill) = TracingSetup::start(Some(TracingBackend::Otlp), &saved);
        assert_eq!(prefill, "http://localhost:4318");
        assert_eq!(step.env_hint(), None);
        let (auth, _) = next(step.submit("http://collector:4318", &saved));
        assert!(auth.masked());
        assert_eq!(
            auth.clone().submit("", &saved),
            Transition::Done(TracingSetupRequest::Otlp {
                endpoint: "http://collector:4318".into(),
                authorization: None,
            })
        );
        assert_eq!(
            auth.submit("Bearer tok", &saved),
            Transition::Done(TracingSetupRequest::Otlp {
                endpoint: "http://collector:4318".into(),
                authorization: Some("Bearer tok".into()),
            })
        );
    }
}
