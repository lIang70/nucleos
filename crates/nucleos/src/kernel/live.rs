//! Watching a turn while it runs.
//!
//! Every turn writes itself into the log one event at a time, and until now nobody could see that
//! happen: `emit` appended to the store and stopped, and `GET /v1/sessions/{id}/events` answers with
//! what is *already* written. So the only way to watch a forty-second turn was to wait forty seconds.
//!
//! This is the missing half, and it is deliberately the smaller half: a broadcast of the events the
//! kernel was already writing.
//!
//! Three properties, each of which is a decision:
//!
//! - **The log stays the truth.** A follower that misses something (a slow reader, a full channel)
//!   catches up by reading the store from the sequence number it has. Nothing is lost by dropping a
//!   notification, which is why `emit` ignores the send error — nobody listening is the normal case.
//! - **The notification carries the event it just wrote**, not a summary and not a copy with a
//!   different shape. A follower therefore sees exactly what a replay would give it, byte for byte,
//!   and cannot learn something the log does not say.
//! - **One channel for every session.** A follower filters by key. Per-session channels would be a
//!   registry to keep in step with the log, and the log already has the key.

use crate::adapter::SessionKey;
use crate::contract::Event;
use tokio::sync::broadcast;

/// How much a follower may fall behind before it is told to catch up from the store.
///
/// Sized for a fast model on a long answer: a stream that lags by this much is a stream whose reader
/// is not keeping up, and the store is the correct answer for that. There is no value that makes
/// lagging impossible, so there is no point pretending otherwise.
pub(crate) const DEPTH: usize = 1024;

/// One written event, and the session it belongs to.
#[derive(Debug, Clone)]
pub struct Live {
    /// Which session wrote it. A follower compares this and skips the rest.
    pub key: SessionKey,
    /// The event, exactly as it was appended.
    pub event: Event,
}

/// Where the kernel announces events, and where a follower subscribes.
#[derive(Debug)]
pub(crate) struct Hub {
    sender: broadcast::Sender<Live>,
}

impl Hub {
    pub(crate) fn new() -> Self {
        Self {
            sender: broadcast::channel(DEPTH).0,
        }
    }

    /// Announce an event. A send that nobody receives is not a failure: the log already has it.
    pub(crate) fn announce(&self, live: Live) {
        let _ = self.sender.send(live);
    }

    pub(crate) fn follow(&self) -> broadcast::Receiver<Live> {
        self.sender.subscribe()
    }
}
