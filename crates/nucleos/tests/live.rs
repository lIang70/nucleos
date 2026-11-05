//! Following a turn while it runs.
//!
//! The property is not "events can be read" — `events()` already did that. It is that an event is
//! delivered *before* the turn that wrote it has finished, which is the only thing that lets a caller
//! see a forty-second answer as it arrives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{Capability, ErrorCode, EventKind, Principal, Routing, SessionId};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;

/// A model that says nothing until it is let go, so "still running" is a fact rather than a race.
struct Gated {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Model for Gated {
    fn capability(&self) -> Capability {
        Capability {
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
        let release = Arc::clone(&self.release);
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            release.notified().await;
            let _ = sender
                .send(Ok(ModelDelta::Text {
                    part_index: 0,
                    text: "late".to_owned(),
                }))
                .await;
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

fn kernel(release: Arc<tokio::sync::Notify>) -> Arc<Kernel> {
    Arc::new(
        Kernel::builder()
            .model("primary", Arc::new(Gated { release }))
            .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
            .policy(Arc::new(AllowAll))
            .clock(Arc::new(FixedClock::at(0)))
            .routing(Routing {
                chain: vec!["primary".to_owned()],
                retries: 0,
            })
            .build()
            .unwrap(),
    )
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: Some(1_000_000),
        ..SessionSpec::default()
    }
}

#[tokio::test]
async fn an_event_arrives_before_the_turn_that_wrote_it_has_finished() {
    let release = Arc::new(tokio::sync::Notify::new());
    let kernel = kernel(Arc::clone(&release));
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    // Subscribed before the turn starts. A follower that attaches afterwards has missed the
    // beginning, which is why the daemon subscribes before it spawns anything.
    let mut live = kernel.follow();
    let turn = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move { kernel.inference(&mut session, &[]).await })
    };

    let first = live.recv().await.unwrap();
    assert!(
        matches!(first.event.kind, EventKind::TurnStarted { .. }),
        "the first thing written is the announcement"
    );
    assert!(
        !turn.is_finished(),
        "the event arrived while the turn was still waiting on the model"
    );

    release.notify_one();
    let mut kinds = Vec::new();
    loop {
        let frame = live.recv().await.unwrap();
        kinds.push(frame.event.kind.type_name().to_owned());
        if matches!(frame.event.kind, EventKind::MessageCompleted { .. }) {
            break;
        }
    }
    assert!(
        kinds.contains(&"nucleos.v1.text_delta".to_owned()),
        "{kinds:?}"
    );
    turn.await.unwrap().unwrap();
}

/// Denies the model and nothing else: the session opens, so the turn can start and then fail.
///
/// `DenyAll` would refuse the session itself — the isolation a caller asks for is gated at `open` — and
/// a test about a turn failing inside the call needs a turn that began.
#[derive(Debug)]
struct DenyModelOnly;

#[async_trait]
impl nucleos::adapter::Policy for DenyModelOnly {
    async fn check(
        &self,
        request: nucleos::adapter::PolicyRequest<'_>,
    ) -> nucleos::adapter::Verdict {
        use nucleos::adapter::{Gated, Verdict};
        match request.subject {
            Gated::Model { .. } => Verdict::Deny {
                code: ErrorCode::Denied,
                reason: "this deployment runs no models".to_owned(),
            },
            _ => Verdict::Allow,
        }
    }
}

#[tokio::test]
async fn a_turn_that_fails_inside_the_call_leaves_a_reason() {
    // The rule: a turn that produced no record must not look like one that is still running. A policy
    // refusal inside the call is the case that used to leave `turn_started` and nothing else — a
    // stream watching it would just stop, which is the ambiguity streaming exists to remove.
    let release = Arc::new(tokio::sync::Notify::new());
    let kernel = Arc::new(
        Kernel::builder()
            .model("primary", Arc::new(Gated { release }))
            .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
            .policy(Arc::new(DenyModelOnly))
            .clock(Arc::new(FixedClock::at(0)))
            .routing(Routing {
                chain: vec!["primary".to_owned()],
                retries: 0,
            })
            .build()
            .unwrap(),
    );
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Denied);

    let events = kernel
        .events(&principal(), &SessionId::new("s1"), 0)
        .await
        .unwrap();
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.type_name()).collect();
    assert_eq!(
        kinds.last(),
        Some(&"nucleos.v1.turn_failed"),
        "the log ends with why: {kinds:?}"
    );
    // And not as a failed attempt: no attempt ran, so inventing one would be a worse lie than
    // silence.
    assert!(!kinds.contains(&"nucleos.v1.attempt_failed"), "{kinds:?}");
}

#[tokio::test]
async fn a_follower_that_attaches_late_is_told_to_read_the_store() {
    // Documented behaviour rather than a promise of completeness: the notification is the fast path,
    // the log is the record. A follower that missed events catches up from a sequence number.
    let release = Arc::new(tokio::sync::Notify::new());
    let kernel = kernel(Arc::clone(&release));
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    release.notify_one();

    kernel.inference(&mut session, &[]).await.unwrap();

    let mut late = kernel.follow();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), late.recv())
            .await
            .is_err(),
        "nothing new happened, so nothing was announced"
    );

    let events = kernel
        .events(&principal(), &SessionId::new("s1"), 0)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::MessageCompleted { .. })),
        "everything it missed is in the log"
    );
}

#[tokio::test]
async fn a_turn_that_fails_before_announcing_itself_writes_why() {
    // No credit: `spend` refuses. The refusal is recorded, so a caller that only has the log still
    // learns what happened — which is what makes the streaming handler's "status code, not a frame"
    // rule safe: nothing is lost by answering with an error instead of opening a stream.
    let release = Arc::new(tokio::sync::Notify::new());
    let kernel = kernel(Arc::clone(&release));
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                // Explicitly none: omitting the field means no limit, which is the opposite of what
                // this test is about.
                credit_tokens: Some(0),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Budget);

    let events = kernel
        .events(&principal(), &SessionId::new("s1"), 0)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::BudgetExhausted { .. })),
        "the refusal is in the log: {:?}",
        events
            .iter()
            .map(|event| event.kind.type_name())
            .collect::<Vec<_>>()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::TurnStarted { .. })),
        "a turn that never ran was never announced"
    );
}
