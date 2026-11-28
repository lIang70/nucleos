use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who said it.
///
/// Four roles, matching every chat dialect. A session's `instructions` are a
/// *field of the request* — the per-call prefix — while a system message is *one
/// message in the transcript*. Both exist because they are not the same thing: a
/// mid-conversation system message means something a hoisted prefix cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Standing instruction carried in the transcript.
    System,
    /// The caller.
    #[default]
    User,
    /// The model.
    Assistant,
    /// A tool's answer to a call the model made.
    Tool,
}

/// One message.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    /// Who said it.
    pub role: Role,
    /// What was said.
    pub content: Content,
    /// What the transport knew and the model does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// Message-scoped annotation.
///
/// Typed where this framework has a meaning for the field, an attribute map where
/// it does not: a value that round-trips as `any` is a value whose type can
/// change on reload.
///
/// **No message id.** An id is transport identity, assigned per request; storing
/// one here would create a second identity for the same message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Metadata {
    /// Ids of earlier units this message continues or cites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_tasks: Vec<String>,
    /// Protocol extension URIs the sender attached.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// Everything else, one JSON value per key.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub attributes: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Metadata {
    /// Whether this would carry nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reference_tasks.is_empty() && self.extensions.is_empty() && self.attributes.is_empty()
    }
}

impl Message {
    /// A message from the caller.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Content::text(text),
            metadata: None,
        }
    }

    /// A message from the model.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Content::text(text),
            metadata: None,
        }
    }

    /// A system message carried in the transcript.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Content::text(text),
            metadata: None,
        }
    }

    /// Check the rules a provider will rely on.
    ///
    /// Two kinds: a message must say something, and a part must appear under the
    /// role that can say it. Both are rejected before any provider I/O — a 400
    /// from upstream is a worse error message than one naming the part.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.content.0.is_empty() {
            return Err(crate::error::Error::invalid("a message must carry content"));
        }
        let mut has_tool_result = false;
        for part in &self.content.0 {
            match part {
                Part::ToolCall { .. } if self.role != Role::Assistant => {
                    return Err(crate::error::Error::invalid(
                        "a tool call may only appear in an assistant message",
                    ));
                }
                Part::Reasoning { .. } if self.role != Role::Assistant => {
                    return Err(crate::error::Error::invalid(
                        "a reasoning part may only appear in an assistant message",
                    ));
                }
                Part::ToolResult { .. } => {
                    has_tool_result = true;
                    if self.role != Role::Tool {
                        return Err(crate::error::Error::invalid(
                            "a tool result may only appear in a tool message",
                        ));
                    }
                }
                _ => {}
            }
        }
        if self.role == Role::Tool && !has_tool_result {
            return Err(crate::error::Error::invalid(
                "a tool message must carry a tool result",
            ));
        }
        Ok(())
    }
}
