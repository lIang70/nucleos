use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where a payload lives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "at", rename_all = "snake_case")]
pub enum MediaSource {
    /// The bytes themselves.
    Inline {
        /// The media type the bytes claim to be.
        media_type: String,
        /// The payload.
        bytes: Vec<u8>,
    },
    /// A URL the consumer fetches.
    Url {
        /// The media type, when known before fetching.
        media_type: String,
        /// Where it lives.
        url: String,
    },
}

impl MediaSource {
    /// The declared media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        match self {
            Self::Inline { media_type, .. } | Self::Url { media_type, .. } => media_type,
        }
    }
}

/// One piece of a message.
///
/// Tagged by `kind` so a tool that ignores the vocabulary (a JSON logger, a
/// policy engine in another language) still round-trips a payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    /// Prose.
    Text {
        /// The text.
        text: String,
    },
    /// An image, inline or by URL.
    Image {
        /// Where it lives.
        source: MediaSource,
    },
    /// Audio.
    Audio {
        /// Where it lives.
        source: MediaSource,
        /// Container format, when the provider names one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        /// Duration, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_millis: Option<u64>,
    },
    /// Video.
    Video {
        /// Where it lives.
        source: MediaSource,
    },
    /// A file that is not media.
    File {
        /// Where it lives.
        source: MediaSource,
        /// The name to show a human.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Structured data: a JSON object a tool returned.
    Data {
        /// The media type, `application/json` when the value is an object.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// The value.
        value: serde_json::Value,
    },
    /// A call the model wants made. It is not the result of one: the kernel
    /// records the call and the host dispatches it in a later request.
    ToolCall {
        /// The call.
        call: ToolCall,
    },
    /// A tool's answer.
    ToolResult {
        /// The result, including its `is_error` flag.
        result: ToolResult,
    },
    /// The model's own reasoning trace.
    ///
    /// A part rather than a marker inside the text, because a caller may want to
    /// store or hide it without parsing. `signature` and `id` are what a
    /// provider expects to be replayed verbatim.
    Reasoning {
        /// The trace.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        /// The provider's signature, for verbatim replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// The provider's id for this block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Which provider produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl Part {
    /// Which kind this is, without the payload.
    #[must_use]
    pub fn kind(&self) -> PartKind {
        match self {
            Self::Text { .. } => PartKind::Text,
            Self::Image { .. } => PartKind::Image,
            Self::Audio { .. } => PartKind::Audio,
            Self::Video { .. } => PartKind::Video,
            Self::File { .. } => PartKind::File,
            Self::Data { .. } => PartKind::Data,
            Self::ToolCall { .. } => PartKind::ToolCall,
            Self::ToolResult { .. } => PartKind::ToolResult,
            Self::Reasoning { .. } => PartKind::Reasoning,
        }
    }
}

/// The part vocabulary, without payloads.
///
/// One enumeration so tables that map kinds to behaviour can be pinned against
/// it: a new kind cannot be added without deciding what it means everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    /// Prose.
    Text,
    /// An image.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// A non-media file.
    File,
    /// Structured data.
    Data,
    /// A call the model wants made.
    ToolCall,
    /// A tool's answer.
    ToolResult,
    /// A reasoning trace.
    Reasoning,
}

impl PartKind {
    /// Every kind, in declaration order.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Text,
            Self::Image,
            Self::Audio,
            Self::Video,
            Self::File,
            Self::Data,
            Self::ToolCall,
            Self::ToolResult,
            Self::Reasoning,
        ]
    }
}
