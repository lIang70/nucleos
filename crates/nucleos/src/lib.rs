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
//! contract → kernel → adapter
//! ```
//!
//! `kernel` is the syscall surface; the loop that drives it lives in `nucleos-agent`, one crate
//! up, because a kernel that runs your application's loop has taken a position on what your
//! application should be.
//!
//! The kernel is driven entirely through [`adapter`], which is what lets most of the tests run
//! without a network. The kernel's own types are also the wire format: [`wire`] holds the HTTP envelopes and
//! the published schema, and no transport appears in this crate.

pub mod adapter;
pub mod contract;
pub mod error;
pub mod kernel;
pub mod wire;

pub use contract::{
    Budget, Capability, Content, Event, EventKind, Generation, Isolation, Message, Part, PartKind,
    PermissionKind, PermissionOption, Principal, Role, SessionId, SessionState, Snapshot,
    StopReason, ToolCall, ToolResult, ToolSpec, Usage,
};
pub use error::{Error, Result};
pub use kernel::Kernel;

/// The kernel's own version.
///
/// The ABI version is a separate thing and lives in [`wire`]: a kernel release may
/// keep speaking an older ABI, and conflating the two is how "we upgraded the daemon and
/// the SDK broke" happens.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
