//! Tools, and the shape a tool source has to speak.

use crate::contract::{Content, Isolation, Part, PartKind, SessionId, TextPart, ToolSpec};
use async_trait::async_trait;

/// One tool, behind the only interface the kernel has.
///
/// The production implementation wraps an MCP client; this trait stays separate so the engine
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
}

impl ToolOutput {
    /// Content that worked.
    #[must_use]
    pub fn ok(content: Content) -> Self {
        Self {
            content,
            is_error: false,
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
        }
    }
}
