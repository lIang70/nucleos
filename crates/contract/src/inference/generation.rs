use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ExtensionSet, FinishReason, ModelRef, Output, Warning};
use super::{Outcome, Usage};
use crate::content::{Content, Message, Part};
use crate::tool::ToolDefinition;

/// One generation call.
///
/// Four `Option`s with meanings that do not overlap, and no second type for "the resolved one".
/// The runtime fills `tools` and `output` before the request reaches a generator; everything else
/// the caller already decided. The same native shape crosses the adapter and wire boundaries.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ModelRequest {
    /// Which model answers.
    pub model: Option<ModelRef>,
    /// The whole conversation, byte for byte as the caller sent it.
    ///
    /// Shared rather than owned: a fallback chain sends one conversation to several models, and
    /// cloning this request increments a reference count instead of copying every media payload.
    pub messages: Arc<[Message]>,
    /// This call's instructions. Separate from messages: it is a prefix, and where a prefix goes
    /// is a dialect question.
    pub instructions: Option<Arc<Content>>,
    /// What the model may call. `None` means whatever the runtime exposed; `Some` narrows that.
    pub tools: Option<Arc<[ToolDefinition]>>,
    /// What to produce. `None` means text.
    pub output: Option<Output>,
    /// Provider-specific options, forwarded untouched.
    pub extensions: ExtensionSet,
    /// Ceiling on generated tokens. Budget reservation is computed from it.
    pub max_output_tokens: Option<u32>,
}

/// One generation call's complete result.
///
/// `outcome` names the answering model rather than repeating the one that was asked for, because a
/// fallback chain may answer with another: a record built from the request would bill a model that
/// never ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GenerateResponse {
    /// Which model actually answered.
    ///
    /// Not the model that was requested: a fallback chain may answer with another one, and a record
    /// built from the request would attribute the spend to a model that never ran. Typed, because a
    /// runtime checks this name against the session's authority.
    pub model: ModelRef,
    /// The assistant turn.
    pub message: Message,
    /// Why it stopped.
    pub finish: FinishReason,
    /// Which model actually answered, and what it spent.
    ///
    /// One value for both halves: a driver that filled them in separately is a driver that could
    /// leave the record unable to say who answered — and under fallback the answering model is not
    /// the requested one.
    pub outcome: Outcome,
    /// Fields the provider could not honour in full.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl GenerateResponse {
    /// Check the accounting a reader and a bill will rely on.
    ///
    /// # Errors
    ///
    /// A breakdown that exceeds its total. Under-reporting leaves the rest unattributed;
    /// over-reporting is a contradiction, and a contradiction is rejected where it is produced
    /// rather than where someone finally adds it up.
    pub fn validate(&self) -> crate::error::Result<()> {
        self.outcome.validate()
    }
}

/// One step of a streaming generation.
///
/// One index space for every content variant: the index is a content part's position, so a tool
/// call is a part at its own index rather than a second sequence an assembler must reconcile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Opens the stream with what is known before any content arrives.
    Start {
        /// The provider's request id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// The provider's response id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// Append text to the part at `index`.
    Text {
        /// Which part.
        index: u32,
        /// The fragment.
        text: String,
    },
    /// Append reasoning text, separately from visible text.
    Reasoning {
        /// Which part.
        index: u32,
        /// The fragment.
        text: String,
        /// A provider signature for verbatim replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// The provider's id for this block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Which provider produced it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Append a fragment of one tool call.
    ToolCall {
        /// Which part.
        index: u32,
        /// The call id, normally on the first fragment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The raw tool-name fragment, normally present only on the first fragment. It remains a
        /// string here because a provider may split it; assembly validates the completed name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Another piece of the argument object.
        args: String,
    },
    /// A whole part arrives at `index`, replacing whatever was there.
    Part {
        /// Which part.
        index: u32,
        /// The part.
        part: Part,
    },
    /// Bytes appended to the media part at `index`.
    ///
    /// The first chunk carries the media template. Later chunks need not repeat it. This is
    /// separate from `Part` because append and replace are different operations.
    Chunk {
        /// Which part.
        index: u32,
        /// The bytes.
        bytes: Vec<u8>,
        /// The template, on the first chunk only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<Part>,
    },
    /// Interim accounting.
    Usage {
        /// What has been spent so far.
        usage: Usage,
    },
    /// Closes the stream. Always last.
    End {
        /// Why it stopped.
        finish: FinishReason,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_request_round_trips_without_copying_its_conversation_between_rust_clones() {
        let request = ModelRequest {
            model: Some(ModelRef::new("openai/gpt-5").unwrap()),
            messages: vec![Message::user("hi")].into(),
            max_output_tokens: Some(100),
            ..ModelRequest::default()
        };
        let copy: ModelRequest = request.clone();
        assert_eq!(copy.messages.len(), 1);
        assert_eq!(Arc::strong_count(&request.messages), 2);

        let json = serde_json::to_string(&request).unwrap();
        let decoded: ModelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn a_stream_event_has_a_stable_kind_tag() {
        let event = StreamEvent::Text {
            index: 2,
            text: "hello".to_owned(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"text""#), "{json}");
        let decoded: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }
}
