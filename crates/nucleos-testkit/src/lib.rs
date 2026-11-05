//! Test doubles, for hosts and for this workspace's own tests.
//!
//! They exist so a kernel test can run without a network, a socket, a process or a sleeping
//! clock. They are a separate crate rather than a module behind a feature flag, so that nothing
//! here can reach a deployed binary: a fake in the production crate is one `use` away from being
//! the deployment.
//!
//! What is *not* here is as deliberate. A policy that allows everything is a real policy with a
//! real meaning, and it lives in `nucleos::adapter`; the store is SQLite, the same code path in a
//! test as in production, and it lives in `nucleos-sqlite`. Only things whose whole purpose is to
//! stand in for something real are in this crate.

mod clock;
mod helpers;
mod model;
mod tool;

pub use clock::FixedClock;
pub use helpers::{one_attempt, text_content};
pub use model::{EchoModel, FakeModel, tool_call_deltas};
pub use tool::FakeTool;
