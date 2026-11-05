//! Where the logs go, and only that.
//!
//! Libraries emit through [`tracing`] and decide nothing about the sink. This module is the other
//! half of that deal: it installs a subscriber, picks the format, and reads the level from the
//! environment. A library that chose its own output would be a library a host cannot embed, and a
//! kernel that wrote files would be a kernel with an opinion about deployments.
//!
//! The choices here are deliberately few:
//!
//! - **JSON, one record per line.** `grep` reads it, a collector parses it, and neither needs a
//!   format negotiation.
//! - **The level is a flag, not a variable.** `--log info`, or any directive `tracing` understands
//!   (`nucleos=debug,nucleos_sqlite=warn`). A daemon whose verbosity comes from the environment is a
//!   daemon whose verbosity is different on the machine you are debugging.
//! - **Stdout.** There is no other stdout output.
//!
//! The event log stays the domain record. What arrives here is the *operational* complement: retries,
//! fallbacks, reconnects, refusals and degradations — the things that never become events because no
//! session owns them.

use tracing_subscriber::EnvFilter;

/// Install the subscriber. Called once, before anything else runs.
///
/// A second call is ignored by the subscriber's own `try_init`, which is the honest behaviour: two
/// installations means a test harness and a binary both wanting the global slot, and the first one
/// winning is better than a panic in the middle of start-up.
pub fn install(directive: &str) {
    let filter = EnvFilter::new(directive);

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .with_target(true)
        .with_writer(std::io::stdout)
        .try_init();
}
