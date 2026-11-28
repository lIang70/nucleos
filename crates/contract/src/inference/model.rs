//! What a model declares about itself.
//!
//! Capability is **configuration, not a probe**: a deployment says what a model
//! can do, and an adapter that discovers otherwise fails somewhere far less
//! obvious than its own constructor.
//!
//! The facts are split three ways because they answer three different questions
//! and have three different lifetimes: what it can do (`Capabilities`), how much
//! it can take (`Limits`), and when it goes away (`Lifecycle`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::id::ModelId;
use crate::content::PartKind;

/// One thing a model can be asked to do.
///
/// Output shapes and operations live in **one** vocabulary on purpose: a grant
/// that says "this model, text only" must not need a second word for "and not
/// embeddings", because two vocabularies eventually disagree.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Produces a text answer.
    Text,
    /// Produces an image.
    Image,
    /// Produces audio.
    Audio,
    /// Produces video.
    Video,
    /// Answers with tool calls. It rides on generation rather than replacing it:
    /// a request that offers tools needs this channel *and* the output it asked
    /// for.
    ToolCall,
    /// Turns documents into vectors.
    Embed,
    /// Answers a batch of typed questions.
    Decide,
}

/// Why a generation stopped.
///
/// The canonical set: a driver maps its provider's vocabulary onto it, and reports an
/// unknown reason as an unusable response rather than inventing one — an invented
/// reason silently bypasses every branch that switches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model completed its turn.
    #[default]
    Stop,
    /// It hit a length limit.
    Length,
    /// It stopped in order to call tools.
    ToolCalls,
    /// The provider withheld output.
    ContentFilter,
    /// It reported an in-band failure.
    Error,
}

/// What a request asks a model to produce.
///
/// The only modality switch in the contract: the same request type, the same
/// response type and the same call serve every output, because the answer is
/// always [`crate::Content`] and its parts say what arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// The next assistant turn: text, tool calls, or both.
    #[default]
    Text,
    /// Generated images.
    Image,
    /// Synthesised speech.
    Audio,
    /// Generated video. Unary at the generation seam even when the provider runs
    /// it as a long task.
    Video,
}

impl Output {
    /// The mode this output requires.
    #[must_use]
    pub fn mode(self) -> Mode {
        match self {
            Self::Text => Mode::Text,
            Self::Image => Mode::Image,
            Self::Audio => Mode::Audio,
            Self::Video => Mode::Video,
        }
    }

    /// The part kind this output produces.
    #[must_use]
    pub fn part_kind(self) -> PartKind {
        match self {
            Self::Text => PartKind::Text,
            Self::Image => PartKind::Image,
            Self::Audio => PartKind::Audio,
            Self::Video => PartKind::Video,
        }
    }
}

/// What a model accepts and emits.
///
/// Zero is the conservative declaration: a model that declares nothing is a text
/// generator that calls no tools.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    /// The input part kinds it accepts.
    ///
    /// Empty means **unknown**, so nothing is filtered — not "accepts nothing".
    /// A declared list rejects an undeclared part before any provider I/O.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PartKind>,
    /// What it can be asked to do. Empty means text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<Mode>,
    /// Default vector width. `Some(n)` also implies [`Mode::Embed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<u32>,
    /// Whether the provider offers its own web search.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hosted_web_search: bool,
}

impl Capabilities {
    /// Every mode this model serves, after defaults.
    ///
    /// Declared modes, plus `Embed` when a width is declared, plus `Text` when
    /// nothing at all was declared. This is the function every reader should
    /// use: `modes` alone under-reports.
    #[must_use]
    pub fn effective_modes(&self) -> Vec<Mode> {
        let mut out: Vec<Mode> = Vec::with_capacity(self.modes.len() + 2);
        for mode in &self.modes {
            if !out.contains(mode) {
                out.push(*mode);
            }
        }
        if self.embedding_dimensions.is_some_and(|width| width > 0) && !out.contains(&Mode::Embed) {
            out.push(Mode::Embed);
        }
        if out.is_empty() {
            out.push(Mode::Text);
        }
        out
    }

    /// Whether it can do one thing.
    #[must_use]
    pub fn can(&self, mode: Mode) -> bool {
        self.effective_modes().contains(&mode)
    }

    /// Whether it accepts one input kind. An undeclared list filters nothing.
    #[must_use]
    pub fn accepts(&self, kind: PartKind) -> bool {
        self.inputs.is_empty() || self.inputs.contains(&kind)
    }
}

/// How much a model can take.
///
/// Every field is optional because `None` means **undeclared**, not zero: the
/// catalog did not claim a value, so a caller must not assume an upper bound.
/// Treating the two as one is how a budget silently stops being enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    /// Cap on input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    /// Cap on output tokens in one response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Cap on a single binary input (an audio payload, say) in bytes. Token
    /// limits say nothing about a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u64>,
}

impl Limits {
    /// Whether a prompt of `tokens` fits, or `None` when nothing was declared.
    #[must_use]
    pub fn fits(&self, tokens: u32) -> Option<bool> {
        self.max_input_tokens.map(|limit| tokens <= limit)
    }
}

/// When a model stops being callable.
///
/// Advertised rather than assumed: a retirement date is the one fact a caller
/// needs before it writes a route naming the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Lifecycle {
    /// Unix milliseconds after which new sessions must not select it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retires_at: Option<u64>,
}

/// Everything a deployment declares about one model.
///
/// There is no operation list: a model whose `inputs` include audio and whose
/// modes are text *is* a transcription service, and no label says so twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelDescriptor {
    /// The name within its vendor.
    pub id: ModelId,
    /// What a human reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What it can do.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// How much it can take.
    #[serde(default)]
    pub limits: Limits,
    /// When it goes away.
    #[serde(default)]
    pub lifecycle: Lifecycle,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_silent_declaration_is_a_text_generator() {
        let capabilities = Capabilities::default();
        assert_eq!(capabilities.effective_modes(), vec![Mode::Text]);
        assert!(capabilities.can(Mode::Text));
        assert!(!capabilities.can(Mode::Image));
        assert!(!capabilities.can(Mode::ToolCall));
    }

    #[test]
    fn a_declared_embedding_width_implies_only_the_embed_mode() {
        let capabilities = Capabilities {
            embedding_dimensions: Some(1024),
            ..Capabilities::default()
        };
        let modes = capabilities.effective_modes();
        assert!(modes.contains(&Mode::Embed));
        assert!(
            !modes.contains(&Mode::Text),
            "a declared width replaces the silent default"
        );
    }

    #[test]
    fn an_undeclared_input_list_filters_nothing() {
        let capabilities = Capabilities::default();
        for kind in PartKind::all() {
            assert!(capabilities.accepts(*kind));
        }
    }

    #[test]
    fn a_declared_input_list_rejects_what_it_omits() {
        let capabilities = Capabilities {
            inputs: vec![PartKind::Text],
            ..Capabilities::default()
        };
        assert!(capabilities.accepts(PartKind::Text));
        assert!(!capabilities.accepts(PartKind::Image));
    }

    #[test]
    fn an_undeclared_limit_is_not_a_zero_limit() {
        let limits = Limits::default();
        assert_eq!(limits.fits(1_000_000), None, "undeclared is not zero");
    }

    #[test]
    fn a_declared_limit_answers() {
        let limits = Limits {
            max_input_tokens: Some(100),
            ..Limits::default()
        };
        assert_eq!(limits.fits(100), Some(true));
        assert_eq!(limits.fits(101), Some(false));
    }
}
