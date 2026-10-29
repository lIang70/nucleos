//! MCP tool sources.
//!
//! The kernel is a *client* of this protocol, not an implementer of it. What it needs is small:
//! `initialize`, `tools/list`, `tools/call`, over the stdio transport (one JSON object per line).
//! That is what this crate speaks, and nothing more — resources, prompts, sampling and elicitation
//! are deliberately absent, and the daemon does not advertise them, so a server that needs them
//! falls back rather than hanging.
//!
//! # Why not `rmcp`
//!
//! The official Rust SDK is a much larger surface (`handler`, `router`, `service`, `transport`) with
//! its own model types, which we would map to ours anyway. What we speak is three methods and a
//! newline delimiter. When this crate needs the rest of the protocol — resources, sampling,
//! elicitation — that is the moment to take the dependency, not before.

pub mod client;
pub mod content;
pub mod source;

pub use client::Client;
pub use source::{McpSource, McpTool};
