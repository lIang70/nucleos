//! The five contracts, and nothing more.
//!
//! Every extension point in the kernel is one of these traits. A sixth needs an argument:
//! each one is something a host must implement, and a host that must implement six things
//! usually implements the sixth badly.
//!
//! | Trait | The question it answers |
//! |---|---|
//! | [`Model`] | How do I turn a prompt into streamed parts? |
//! | [`Tool`] | How do I execute one call? |
//! | [`Store`] | Where do events and snapshots live? |
//! | [`Policy`] | Is this call allowed? |
//! | [`Clock`] | What time is it, for a test? |
//!
//! Two shapes are worth stating because they are the ones an adapter author will want to
//! change first:
//!
//! - **`Tool::invoke` returns no `Result`.** A failed call is content with `is_error` set,
//!   because the error text is what the model must read in order to recover. Returning
//!   `Err` would leave the model with a failure it cannot see.
//! - **`Policy::check` is async.** A real policy evaluates against something — a document, a
//!   database, a remote service — and a synchronous signature pushes implementors toward
//!   blocking a runtime worker.
//!
//! Two of these ship with an implementation rather than only a trait: [`SystemClock`], because
//! reading the time is not a decision, and [`policy::AllowAll`] / [`policy::DenyAll`], because
//! "no policy configured" and "refuse everything" are real answers rather than stand-ins.
//!
//! Nothing else does. A test double in this crate is one `use` away from being the deployment, so
//! the doubles live in `nucleos-testkit`, which no binary depends on, and the store lives in
//! `nucleos-sqlite`, where a test and a deployment run the same code.

pub mod clock;
pub mod model;
pub mod policy;
pub mod registry;
pub mod store;
pub mod tool;

pub use clock::{Clock, SystemClock};
pub use model::{
    FinishReason, GenerateParams, Model, ModelDelta, ModelRequest, ModelStream, ReasoningEffort,
    TextFormat, ToolChoice,
};
pub use policy::{AllowAll, ConfigScope, DenyAll, Gated, Policy, PolicyRequest, Verdict};
pub use registry::ModelRegistry;
pub use store::{PruneQuery, PruneReport, SessionKey, SessionPage, SessionQuery, Store};
pub use tool::{NoProgress, Progress, Tool, ToolCtx, ToolOutput};
