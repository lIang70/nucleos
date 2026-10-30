//! The reference agent loop, built on the kernel's syscalls.
//!
//! This is a driver, not a kernel. Everything it does is something an application could do
//! itself: open a session, ask for an inference, run the tools it asked for, append the results,
//! repeat until a stop condition fires, close.
//!
//! It exists because the loop's subtle rules are the part nobody should have to rediscover —
//! an attempt that produced content is never retried, a batch asks policy before it runs, broken
//! arguments never reach a tool, and stopping is four layers deep. An application that wants a
//! different loop should write one; an application that wants *this* loop should not write it
//! again.

mod agent;

pub use agent::{Step, answer, drive, resume, start, step, text_message};
pub use nucleos::kernel::Outcome;
