//! The `nucleos` command line.
//!
//! Two halves, and which half a command belongs to is the whole design:
//!
//! - **local** commands touch this machine's files — `init` creates the stores, the recovery password
//!   and the administrator's key. Nothing here talks to a daemon, because before one is running there
//!   is nothing to talk to.
//! - **remote** commands are requests over the daemon's socket: adding a model, replacing the policy,
//!   issuing a token. They arrive with the administrative ABI, and they are signed — the CLI is a
//!   client of the same ABI a script or `curl` would use, not a second authority that edits files.
//!
//! Keeping the split explicit is what stops the CLI from becoming a second source of truth: it can
//! create a deployment, and after that it can only ask.

pub mod client;
pub mod config;
pub mod init;
pub mod models;
pub mod remote;

pub use init::{Done, Options, run};
