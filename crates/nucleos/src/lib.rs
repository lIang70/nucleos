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
//! Nothing here knows about protobuf, sockets, or authentication. The wire contract lives
//! in `nucleos-proto` and is translated in `nucleosd`; see `docs/abi.md`.
//!
//! The engine lands next; the contracts it will drive are in [`adapter`].

pub mod adapter;
pub mod contract;
pub mod engine;
pub mod error;

pub use contract::{
    Budget, Capability, Content, Event, EventKind, Generation, Isolation, Message, Part, PartKind,
    PermissionKind, PermissionOption, Principal, Role, SessionId, SessionState, Snapshot,
    StopReason, ToolCall, ToolResult, ToolSpec, Usage,
};
pub use error::{Error, Result};

/// The kernel's own version.
///
/// The ABI version is a separate thing and lives in `nucleos-proto`: a kernel release may
/// keep speaking an older ABI, and conflating the two is how "we upgraded the daemon and
/// the SDK broke" happens.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
