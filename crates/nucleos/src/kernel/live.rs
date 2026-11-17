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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::contract::{Event, EventKind, SessionState};

    fn event(seq: u64) -> Event {
        Event {
            seq,
            session: crate::contract::SessionId::new("s1"),
            at_unix_millis: 0,
            schema_version: Event::SCHEMA_VERSION,
            kind: EventKind::StateChanged {
                state: SessionState::Open,
                stop: None,
            },
            meta: Default::default(),
        }
    }

    fn live(seq: u64) -> Live {
        Live {
            key: crate::adapter::SessionKey::new("acme", crate::contract::SessionId::new("s1")),
            event: event(seq),
        }
    }

    #[tokio::test]
    async fn a_follower_receives_what_was_announced() {
        let hub = Hub::new();
        let mut follower = hub.follow();
        hub.announce(live(1));
        hub.announce(live(2));
        let first = follower.recv().await.unwrap();
        let second = follower.recv().await.unwrap();
        assert_eq!(first.event.seq, 1);
        assert_eq!(second.event.seq, 2);
    }

    #[tokio::test]
    async fn announcing_with_no_followers_is_not_an_error() {
        let hub = Hub::new();
        // No followers: the send fails inside `announce`, and the hub ignores it. The log is the
        // record; the broadcast is only ever the fast path.
        hub.announce(live(1));
        hub.announce(live(2));
        // A follower that subscribes now sees only what came after.
        let mut follower = hub.follow();
        hub.announce(live(3));
        let received = follower.recv().await.unwrap();
        assert_eq!(received.event.seq, 3);
    }

    #[tokio::test]
    async fn a_slow_follower_is_told_it_lagged_rather_than_silently_skipping() {
        let hub = Hub::new();
        let mut follower = hub.follow();
        // Fill the channel past its depth. The follower has not read anything, so the oldest
        // events are overwritten.
        for seq in 0..(DEPTH as u64 + 10) {
            hub.announce(live(seq));
        }
        // The first read is a Lagged error, not an event. That is the signal to catch up from the
        // store: the follower learns it missed something, rather than believing it saw everything.
        match follower.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => {
                assert!(n > 0, "reports how many were overwritten");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_followers_each_see_every_event() {
        let hub = Hub::new();
        let mut first = hub.follow();
        let mut second = hub.follow();
        hub.announce(live(1));
        hub.announce(live(2));
        for follower in [&mut first, &mut second] {
            assert_eq!(follower.recv().await.unwrap().event.seq, 1);
            assert_eq!(follower.recv().await.unwrap().event.seq, 2);
        }
    }
}
