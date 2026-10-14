//! Requests a backend makes of the user, and the answers.
//!
//! Every variant keeps the backend's own ids verbatim: a display label is not
//! a valid reply, so the surface answers with the native id it was given.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::InteractionId;
use crate::update::ToolCallUpdate;

/// Something the backend needs from the user before it can continue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionRequest {
    /// Approve or reject a tool call.
    Permission {
        /// Interaction id.
        id: InteractionId,
        /// The tool call awaiting approval, when the backend names one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call: Option<ToolCallUpdate>,
        /// The choices the backend offers, in its order.
        options: Vec<PermissionOption>,
    },
    /// A free-text or multiple-choice question.
    Question {
        /// Interaction id.
        id: InteractionId,
        /// Question text.
        prompt: String,
        /// Offered answers. `None` means free text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choices: Option<Vec<Choice>>,
    },
    /// A structured form. The answer is a JSON value matching `schema`.
    Form {
        /// Interaction id.
        id: InteractionId,
        /// Form title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// JSON Schema of the expected answer.
        schema: Value,
    },
    /// The user must open a URL, for example to sign in.
    Url {
        /// Interaction id.
        id: InteractionId,
        /// URL to open.
        url: String,
        /// Why the backend needs it.
        purpose: String,
    },
    /// A request this Loop version cannot present. It is answered with an
    /// explicit error, never left pending.
    Unsupported {
        /// Interaction id.
        id: InteractionId,
        /// The backend's method or request type.
        method: String,
        /// The raw request.
        raw: Value,
    },
}

impl InteractionRequest {
    /// The interaction id.
    pub fn id(&self) -> &InteractionId {
        match self {
            Self::Permission { id, .. }
            | Self::Question { id, .. }
            | Self::Form { id, .. }
            | Self::Url { id, .. }
            | Self::Unsupported { id, .. } => id,
        }
    }
}

/// One choice in a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// The backend's own option id, sent back unchanged.
    pub native_id: String,
    /// Label to show.
    pub label: String,
    /// What the option means, for presentation only. Loop does not infer a
    /// grant or suppress later prompts from it: the backend defines the scope.
    pub kind_hint: PermissionKind,
}

/// The meaning of a permission option, as a presentation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Allow this call.
    AllowOnce,
    /// Allow this call and remember the choice.
    AllowAlways,
    /// Reject this call.
    RejectOnce,
    /// Reject this call and remember the choice.
    RejectAlways,
    /// A kind this reader does not know.
    #[serde(other)]
    Other,
}

/// One choice in a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// The backend's own choice id.
    pub native_id: String,
    /// Label to show.
    pub label: String,
}

/// The user's answer to an interaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum InteractionOutcome {
    /// A permission option or question choice, by its native id.
    Selected {
        /// The chosen native id.
        native_id: String,
        /// Optional note from the user, such as why they rejected a call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// A free-text or form answer.
    Answered {
        /// The answer.
        value: Value,
    },
    /// The interaction was withdrawn, for example because the turn was cancelled.
    Cancelled,
    /// Loop could not present the interaction.
    Rejected {
        /// Why.
        reason: String,
    },
}
