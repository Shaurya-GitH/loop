//! Shared plumbing for the TUI setup wizards (`/login`, `/tracing setup`).
//!
//! A wizard is a pure state machine: each step is shown through [`WizardView`], and
//! submitting the input line yields a [`Transition`].

/// Result of submitting the input line at a wizard step.
#[derive(Debug, PartialEq, Eq)]
pub enum Transition<Step, Request> {
    /// Show `step`, with the input line pre-filled.
    Next {
        /// Next step.
        step: Step,
        /// Suggested input (saved value or default).
        prefill: String,
    },
    /// Input rejected; stay on `step` and show `error`.
    Retry {
        /// Same step.
        step: Step,
        /// Why the input was rejected.
        error: String,
    },
    /// All fields collected.
    Done(Request),
}

/// How the setup box renders a wizard step.
pub trait WizardView {
    /// Heading.
    fn title(&self) -> String;
    /// What to enter at this step.
    fn instructions(&self) -> String;
    /// Placeholder while the input line is empty.
    fn placeholder(&self) -> &'static str;
    /// Picker rows `(label, description)` and the highlighted row, on choice steps.
    fn options(&self) -> Option<(Vec<(String, String)>, usize)> {
        None
    }
    /// Whether the input line is hidden (secrets).
    fn masked(&self) -> bool {
        false
    }
    /// Env var alternative, where one exists.
    fn env_hint(&self) -> Option<String> {
        None
    }
    /// Move the picker highlight; no-op outside choice steps.
    fn move_selection(&mut self, _delta: isize) {}
}

/// Wrap-around picker movement.
pub fn wrap_selection(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (selected as isize + delta).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_wraps_both_ways() {
        assert_eq!(wrap_selection(0, -1, 3), 2);
        assert_eq!(wrap_selection(2, 1, 3), 0);
        assert_eq!(wrap_selection(1, 1, 3), 2);
        assert_eq!(wrap_selection(0, 1, 0), 0);
    }
}
