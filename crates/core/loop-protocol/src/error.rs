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
}

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ErrorInfo {}
