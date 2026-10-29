//! Tools, and the shape a tool source has to speak.

use crate::contract::{Content, Isolation, Part, PartKind, SessionId, TextPart, ToolSpec, Usage};
use async_trait::async_trait;

/// One tool, behind the only interface the kernel has.
///
/// The production implementation wraps an MCP client; this trait stays separate so the kernel
/// can be tested against a fake that spawns nothing.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The declaration the model sees.
    fn spec(&self) -> ToolSpec;

    /// How strongly execution is separated from the kernel.
    ///
    /// Declared, never verified here: whoever launches the tool enforces it, and the kernel's
    /// only use for it is refusing to migrate a session into a generation where it changed.
    fn isolation(&self) -> Isolation {
        Isolation::None
    }

    /// Run one call.
    ///
    /// There is no `Result`. A tool that fails returns content with
    /// [`ToolOutput::is_error`] set, because the error text is what the model must read to
    /// recover — a failure outside the content model is a failure the model cannot see.
    async fn invoke(&self, args_json: &str, cx: &ToolCtx<'_>) -> ToolOutput;
}

/// What a tool may know about the call it is answering.
///
/// Deliberately narrow. A tool that can reach the session has a way to change state the
/// kernel believes it owns.
#[derive(Debug)]
pub struct ToolCtx<'a> {
    /// The session this call belongs to.
    pub session: &'a SessionId,
    /// The correlation id the result must be filed under.
    pub call_id: &'a str,
    /// The wall-clock deadline for this call, in unix milliseconds.
    pub deadline_unix_millis: u64,
}

/// What a tool answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// The content, on success and on failure alike.
    pub content: Content,
    /// Whether the call failed.
    pub is_error: bool,
    /// What the call cost, when the tool knows.
    ///
    /// A tool that calls a paid API can say so, and the kernel folds it into the session's spend
    /// and its ledger. A tool that reports nothing stays unreported — the same rule model usage
    /// follows, and the reason the ledger never quietly counts an unknown as zero.
    ///
    /// Money a tool spends without saying so is money the kernel cannot attribute. That is a
    /// limitation of the design, not an oversight: the alternative was a gateway endpoint, which
    /// would have been a second product.
    pub usage: Option<Usage>,
}

impl ToolOutput {
    /// Content that worked.
    #[must_use]
    pub fn ok(content: Content) -> Self {
        Self {
            content,
            is_error: false,
            usage: None,
        }
    }

    /// A plain-text answer that worked.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::ok(Content::new([Part::new(PartKind::Text(TextPart {
            text: text.into(),
        }))]))
    }

    /// Content that says why it failed.
    ///
    /// The text is for the model, which is why it is content and not a log line.
    #[must_use]
    pub fn failed(text: impl Into<String>) -> Self {
        Self {
            content: Content::new([Part::new(PartKind::Text(TextPart { text: text.into() }))]),
            is_error: true,
            usage: None,
        }
    }

    /// Declare what this call cost.
    #[must_use]
    pub fn reporting_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}
