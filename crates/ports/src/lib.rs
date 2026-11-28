//! The seams: what a host must implement, and nothing more.
//!
//! Every extension point is one of these traits or one of these values. A new one
//! needs an argument: each is something a host must implement, and a host that
//! must implement seven things usually implements the seventh badly.
//!
//! # The seams
//!
//! | Seam | The question it answers |
//! |---|---|
//! | [`Generator`] | How do I turn a request into an answer? |
//! | [`Embedder`] | How do I turn documents into vectors? |
//! | [`Decider`] | How do I answer a batch of typed questions? |
//! | [`Tool`] | How do I run one call? |
//! | [`Models`] | How does a tool call a model? |
//! | [`Runner`] | How do I start and manage a process session? |
//! | [`Journal`] | Append one fact, store one blob |
//! | [`History`] | Read the log, read the blob |
//! | [`Clock`] | What time is it, for a test? |
//!
//! [`Store`] is not a seam of its own: it is the pair `Journal + History`, so a reader cannot reach
//! `append` and a test that only reads implements one trait.
//!
//! # Two shapes worth stating
//!
//! **`Tool::execute` returns `Result`, and dispatch never returns one.** The
//! invariant is that the model always reads a failure, not that an author gives up
//! `?` — so the conversion happens once, in dispatch, and nowhere else.
//!
//! **A seam a provider does not serve is `None`, not a stub.** [`Provider`] is a
//! struct with optional fields for exactly that reason: a stub that always reports
//! "unsupported" makes every provider carry code for what it cannot do.
//!
//! # What is not here
//!
//! No adapter implementation lives here. The only concrete value is an answer rather than a
//! stand-in: [`SystemClock`], because reading the time is not a decision. Everything else lives
//! where a test and a deployment run the same code.

pub mod clock;
pub mod model;
pub mod sandbox;
pub mod store;
pub mod tool;

pub use clock::{Clock, SystemClock};
pub use model::{
    Catalog, DecideRequest, Decider, Decision, EmbedRequest, EmbedResponse, Embedder, Generator,
    ModelRequest, Provider, Stream, StreamEvent,
};
pub use sandbox::{
    OutputChunk, Runner, Session, SessionInfo, SessionOutput, SessionStarter, SessionStream,
};
pub use store::{History, Journal, Store};
pub use tool::{
    Allow, Binding, CapSet, CapView, Capability, FileTree, Kind, LazyTool, Models, Nested,
    SkillIndex, Source, Tool, ToolMeta, ToolSession, View,
};
