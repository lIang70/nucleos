use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An ordered run of parts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Content(pub Vec<Part>);

impl Content {
    /// Wrap parts.
    #[must_use]
    pub fn new(parts: impl IntoIterator<Item = Part>) -> Self {
        Self(parts.into_iter().collect())
    }

    /// One text part.
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self(vec![Part::Text { text: value.into() }])
    }

    /// Concatenated text parts. Media and tool parts contribute nothing: the
    /// answer's prose is what this is for.
    #[must_use]
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Text { text } | Part::Reasoning { text, .. } => out.push_str(text),
                _ => {}
            }
        }
        out
    }

    /// Whether there is nothing here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The tool calls this content asks for, in order.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.0
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall { call } => Some(call),
                _ => None,
            })
            .collect()
    }
}
