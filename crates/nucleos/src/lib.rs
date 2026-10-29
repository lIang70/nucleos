//! An agent OS kernel.
//!
//! The kernel's job is to be *correct*: a session's state never goes backwards, a budget
//! is never exceeded, an event is never lost. Everything about policy — which model,
//! which tools, who may call what, how much anything costs — is injected through the
//! traits in [`adapter`].
//!
//! The module graph is one-way and is pinned by `tests/layering.rs`:
//!
//! ```text
//! contract → engine → adapter → (host)
//! ```
//!
//! The engine is driven entirely through [`adapter`], which is what lets most tests run without a
//! network. The kernel's own types are also the wire format: [`wire`] holds the HTTP envelopes and
//! the published schema, and no transport appears in this crate.

pub mod adapter;
pub mod contract;
pub mod engine;
pub mod error;
pub mod wire;

pub use contract::{
    Budget, Capability, Content, Event, EventKind, Generation, Isolation, Message, Part, PartKind,
    PermissionKind, PermissionOption, Principal, Role, SessionId, SessionState, Snapshot,
    StopReason, ToolCall, ToolResult, ToolSpec, Usage,
};
pub use error::{Error, Result};

/// The kernel's own version.
///
/// The ABI version is a separate thing and lives in [`wire`]: a kernel release may
/// keep speaking an older ABI, and conflating the two is how "we upgraded the daemon and
/// the SDK broke" happens.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
