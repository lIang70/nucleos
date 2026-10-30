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
//! - **`NUCLEOS_LOG`, falling back to `RUST_LOG`, defaulting to `info`.** One filter, in the shape
//!   the ecosystem already documents — `nucleos=debug,nucleos_sqlite=warn` works without us writing
//!   a filter language.
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
pub fn install() {
    let filter = EnvFilter::try_from_env("NUCLEOS_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .with_target(true)
        .with_writer(std::io::stdout)
        .try_init();
}
