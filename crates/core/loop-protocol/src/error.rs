//! Typed failure information for turns.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a turn failed, as far as Loop can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// Sign-in missing, expired or rejected.
    Auth,
    /// The plan's usage allowance is used up. Retrying will not help until it resets.
    Quota,
    /// Temporarily throttled. Retrying later can succeed.
    RateLimit,
    /// The backend or provider could not be reached, or the connection dropped.
    Network,
    /// A message violated the protocol, or versions do not match.
    Protocol,
    /// The agent or model reported a failure.
    Agent,
    /// The user or caller cancelled the turn.
    Cancelled,
    /// A bug or unexpected state inside Loop.
    Internal,
    /// A category this reader does not know.
    #[serde(other)]
    Other,
}

/// A failure with enough structure for a surface to decide what to do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Failure category.
    pub category: ErrorCategory,
    /// Whether retrying the same request can succeed.
    pub retryable: bool,
    /// Human-readable description. Never contains secrets.
    pub message: String,
}

impl ErrorInfo {
    /// An error whose retryability follows from its category.
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        let retryable = matches!(category, ErrorCategory::RateLimit | ErrorCategory::Network);
        Self {
            category,
            retryable,
            message: message.into(),
        }
    }

    /// Best-effort category for a failure only known as text, such as a
    /// provider's HTTP error line.
    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_ascii_lowercase();
        let has = |needle: &str| lower.contains(needle);
        let category = if has("http 401")
            || has("http 403")
            || has("unauthorized")
            || has("api key")
            || has("not logged in")
            || has("authentication")
        {
            ErrorCategory::Auth
        } else if has("insufficient_quota") || has("quota") || has("billing") || has("usage limit")
        {
            ErrorCategory::Quota
        } else if has("http 429") || has("rate limit") || has("too many requests") {
            ErrorCategory::RateLimit
        } else if has("http 5")
            || has("timed out")
            || has("timeout")
            || has("connection")
            || has("network")
        {
            ErrorCategory::Network
        } else {
            ErrorCategory::Agent
        };
        Self::new(category, message)
    }
}

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ErrorInfo {}
