//! MCP tool sources.
//!
//! The kernel is a *client* of this protocol, not an implementer of it. What it needs is small:
//! `initialize`, `tools/list`, `tools/call`, over the stdio transport (one JSON object per line).
//! That is what this crate speaks, and nothing more — resources, prompts, sampling and elicitation
//! are deliberately absent, and the daemon does not advertise them, so a server that needs them
//! falls back rather than hanging.
//!
//! # Why `rmcp`
//!
//! Hand-rolling this was a mistake worth naming: the three calls a happy path needs are easy, and
//! everything around them is not. A hand-written client has no cancellation — a tool that times out
//! keeps running on the server — no progress, no `tools/list_changed`, no protocol version
//! negotiation, and no way to reach a remote server over HTTP. A runtime whose sessions have
//! budgets and cancellation cannot be built on a client that cannot cancel.
//!
//! What stays ours is the part that is ours: [`content`] maps the protocol's blocks onto the
//! kernel's content model, and [`McpTool`] is an ordinary [`Tool`](nucleos::adapter::Tool).

/// The protocol crate, re-exported so a caller does not need a second dependency line.
pub use rmcp;

pub mod content;
pub mod source;

pub use source::{McpSource, McpTool};
