//! The TUI setup box: whichever wizard is currently collecting input.

use crate::agent_question::AgentQuestion;
use crate::provider_setup::ProviderSetup;
#[cfg(feature = "telemetry")]
use crate::tracing_setup::TracingSetup;
use crate::wizard::WizardView;

/// What the setup box is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupPrompt {
    /// `/login` (also the first-run setup).
    Provider(ProviderSetup),
    /// `/tracing setup`.
    #[cfg(feature = "telemetry")]
    Tracing(TracingSetup),
    /// A question, form or link from the agent.
    Question(AgentQuestion),
}

impl SetupPrompt {
    fn view(&self) -> &dyn WizardView {
        match self {
            Self::Provider(step) => step,
            #[cfg(feature = "telemetry")]
            Self::Tracing(step) => step,
            Self::Question(step) => step,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Provider(_) => "login",
            #[cfg(feature = "telemetry")]
            Self::Tracing(_) => "tracing setup",
            Self::Question(_) => "agent question",
        }
    }

    /// Heading of the setup box.
    pub fn title(&self) -> String {
        self.view().title()
    }

    /// What to enter.
    pub fn instructions(&self) -> String {
        self.view().instructions()
    }

    /// Env var alternative, if any.
    pub fn env_hint(&self) -> Option<String> {
        self.view().env_hint()
    }

    /// Picker rows and the highlighted row, on choice steps.
    pub fn options(&self) -> Option<(Vec<(String, String)>, usize)> {
        self.view().options()
    }

    /// Whether the input line is hidden.
    pub fn masked(&self) -> bool {
        self.view().masked()
    }

    /// Placeholder while the input line is empty.
    pub fn placeholder(&self) -> &'static str {
        self.view().placeholder()
    }

    /// Move the picker highlight.
    pub fn move_selection(&mut self, delta: isize) {
        match self {
            Self::Provider(step) => step.move_selection(delta),
            #[cfg(feature = "telemetry")]
            Self::Tracing(step) => step.move_selection(delta),
            Self::Question(step) => step.move_selection(delta),
        }
    }

    /// Footer / status-line hint; `first_run` as for [`Self::esc_quits`].
    pub fn status_hint(&self, first_run: bool) -> String {
        let keys = if self.options().is_some() {
            "↑↓ choose · enter continue"
        } else if self.masked() {
            "enter save"
        } else {
            "enter next"
        };
        let esc = if self.esc_quits(first_run) {
            "quit"
        } else {
            "cancel"
        };
        format!("{} · {keys} · esc {esc}", self.label())
    }

    /// Prefix for errors shown while this prompt is open.
    pub fn error_prefix(&self) -> &'static str {
        self.label()
    }

    /// Status line after esc.
    pub fn cancelled_message(&self) -> String {
        match self {
            Self::Question(_) => "question dismissed; the agent was told".into(),
            _ => format!("{} cancelled", self.label()),
        }
    }

    /// The agent's interaction this prompt answers, if it is one.
    pub fn interaction(&self) -> Option<&loop_protocol::InteractionId> {
        match self {
            Self::Question(step) => Some(step.id()),
            _ => None,
        }
    }

    /// Whether esc quits the app (first-run provider setup) instead of cancelling.
    pub fn esc_quits(&self, first_run: bool) -> bool {
        first_run && matches!(self, Self::Provider(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prompt_delegates_to_login_wizard() {
        let prompt =
            SetupPrompt::Provider(ProviderSetup::start(None, Vec::new(), Vec::new()).unwrap());
        assert_eq!(
            prompt.title(),
            "Connect a model provider, subscription or web search"
        );
        assert!(prompt.options().is_some());
        assert!(prompt.esc_quits(true));
        assert!(!prompt.esc_quits(false));
        assert_eq!(prompt.cancelled_message(), "login cancelled");
        assert!(prompt.status_hint(false).starts_with("login · ↑↓ choose"));
        assert!(prompt.status_hint(true).ends_with("esc quit"));

        let key = SetupPrompt::Provider(
            ProviderSetup::start(Some("openai"), Vec::new(), Vec::new()).unwrap(),
        );
        assert!(key.masked());
        assert_eq!(key.env_hint(), None);
        assert!(key.status_hint(false).contains("enter save"));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn tracing_prompt_delegates_to_tracing_wizard() {
        use loop_app_core::config::TracingSettings;

        let (step, _) = TracingSetup::start(None, &TracingSettings::default());
        let mut prompt = SetupPrompt::Tracing(step);
        assert_eq!(prompt.title(), "Set up tracing");
        assert!(!prompt.esc_quits(true));
        assert_eq!(prompt.cancelled_message(), "tracing setup cancelled");
        prompt.move_selection(1);
        assert_eq!(prompt.options().unwrap().1, 1);
    }
}
