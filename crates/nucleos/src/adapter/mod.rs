//! The five contracts, and nothing more.
//!
//! Every extension point in the kernel is one of these traits. A sixth needs an argument:
//! each one is something a host must implement, and a host that must implement six things
//! will usually implement the sixth badly.
//!
//! | Trait | The question it answers |
//! |---|---|
//! | [`Model`] | How do I turn a prompt into streamed parts? |
//! | [`Tool`] | How do I execute one call? |
//! | [`Store`] | Where do events and snapshots live? |
//! | [`Policy`] | Is this call allowed? |
//! | [`Clock`] | What time is it, for a test? |
//!
//! Only [`Tool`] has a production implementation planned (an MCP client, see
//! `docs/abi.md`). The trait stays separate so the engine can be tested against a fake
//! that does not spawn a process.
//!
//! The signatures land in P1. They are sketched here because the shape — not the
//! signature — is what the rest of the design depends on:
//!
//! ```text
//! Model::capability() -> Capability            // tools? stream? context window?
//! Model::invoke(&req) -> ModelStream
//! Tool::spec() -> ToolSpec
//! Tool::invoke(args, cx) -> ToolOutcome
//! Store::append(sid, &[Event]) -> Result<()>
//! Store::snapshot(sid) -> Result<Option<Snapshot>>
//! Store::events_since(sid, seq) -> Result<Vec<Event>>
//! Policy::check(&call, &Principal) -> Verdict   // Allow | Deny | AskHuman
//! Clock::now() -> Instant
//! ```
