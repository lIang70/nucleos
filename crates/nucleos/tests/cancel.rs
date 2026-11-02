//! Cancelling a turn that is already running.
//!
//! A budget bounds a turn and a deadline bounds a session; neither stops one *now*. The distinction
//! these tests pin: cancelling stops a call and leaves the session open, the spend already reported
//! stays debited, and the attempt is recorded as cancelled rather than as a bug.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{ErrorCode, EventKind, Principal, Routing, SessionId};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;

/// A model that takes its time, so a cancel can arrive while it is running.
struct Slow;

#[async_trait]
impl Model for Slow {
    fn capability(&self) -> nucleos::contract::Capability {
        nucleos::contract::Capability {
            tools: true,
            tool_choice_required: true,
            structured_output: true,
            streaming: true,
            context_window: 128_000,
            max_output_tokens: 4_096,
        }
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(&self, _request: ModelRequest) -> nucleos::error::Result<ModelStream> {
        // A stream that produces something and then stalls: the shape of a model that is looping.
        let deltas = vec![Ok(ModelDelta::Text {
            part_index: 0,
            text: "thinking".to_owned(),
        })];
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<nucleos::error::Result<ModelDelta>>(4);
        tokio::spawn(async move {
            for delta in deltas {
                let _ = sender.send(delta).await;
            }
            // Never finishes on its own.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = sender
                .send(Ok(ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }))
                .await;
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Arc<Kernel>, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(Slow))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (Arc::new(kernel), store)
}

#[tokio::test]
async fn cancelling_stops_the_turn_and_leaves_the_session_open() {
    let (kernel, store) = kernel();
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: 1_000_000,
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    let id = session.id().clone();
    let mut session = session;

    // Cancel from another task once the turn has started. `begin_turn` happens inside `inference`, so
    // the cancel arrives a moment after it registers; the signal is not lost because `cancelled()`
    // checks the flag before it waits.
    let canceller = {
        let kernel = Arc::clone(&kernel);
        let store = Arc::clone(&store);
        let id = id.clone();
        let key = session.key();
        tokio::spawn(async move {
            // Wait for the model to have said something. Cancelling before the first delta would make
            // "what survives the cancel" a question about timing rather than about the design.
            for _ in 0..200 {
                let spoken = store
                    .events_since(&key, 0)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event.kind, EventKind::TextDelta { .. }));
                if spoken {
                    return kernel.cancel(&nucleos::adapter::SessionKey::new("acme", id));
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("the model never produced a delta; the fixture is wrong");
        })
    };

    let result = kernel.inference(&mut session, &[]).await;
    let cancelled = canceller.await.unwrap();
    assert!(cancelled, "the turn was registered and cancelled");

    let error = result.expect_err("a cancelled turn does not answer");
    assert_eq!(error.code(), ErrorCode::Cancelled);

    // Open, not closed: cancelling stops a call, it does not end a conversation.
    assert!(!session.is_closed());

    // The attempt is in the log as *cancelled*, which is what tells the next morning's reader that
    // nothing went wrong — somebody decided.
    let events = store.events_since(&session.key(), 0).await.unwrap();
    let attempt = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::AttemptFailed { attempt } => Some(attempt.clone()),
            _ => None,
        })
        .expect("the cancelled attempt is recorded");
    assert!(matches!(
        attempt.outcome,
        nucleos::contract::AttemptOutcome::Failed {
            code: ErrorCode::Cancelled,
            ..
        }
    ));

    // And what the model had said before the cancel is still in the stream.
    assert!(events.iter().any(
        |event| matches!(&event.kind, EventKind::TextDelta { text, .. } if text == "thinking")
    ));
}

#[tokio::test]
async fn cancelling_a_session_that_is_not_running_says_nothing_happened() {
    let (kernel, _store) = kernel();
    let session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await
        .unwrap();
    // Not an error: a caller racing the end of a turn is a normal race, not a mistake.
    assert!(!kernel.cancel(&session.key()));
}
