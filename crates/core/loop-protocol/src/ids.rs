//! Opaque identifiers. Each is a string on the wire.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
    /// One configured backend: an agent kind plus its account and settings.
    BackendInstanceId
);
string_id!(
    /// A conversation as Loop knows it. Stable across process restarts.
    SessionKey
);
string_id!(
    /// One user prompt and everything the agent does in response.
    TurnId
);
string_id!(
    /// One user or agent message within a turn.
    MessageId
);
string_id!(
    /// One tool call, as named by the backend.
    ToolCallId
);
string_id!(
    /// One pending interaction (permission prompt, question, form or URL flow).
    InteractionId
);
