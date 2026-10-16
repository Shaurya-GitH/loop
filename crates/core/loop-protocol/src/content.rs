//! Content blocks carried by messages, thoughts and tool output.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A unit of content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// Text payload.
        text: String,
    },
    /// Base64-encoded image.
    Image {
        /// Base64 data.
        data: String,
        /// MIME type, such as `image/png`.
        mime_type: String,
    },
    /// A reference to a resource the reader may fetch or open.
    ResourceLink {
        /// Resource URI.
        uri: String,
        /// Display name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A block with a `type` this reader does not know. The raw JSON is kept.
    #[serde(untagged)]
    Unknown(Value),
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The text of a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}
