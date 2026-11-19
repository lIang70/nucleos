//! A stream is a promise, and these are the ways a provider can break it.
//!
//! Three failures used to be invisible, because each one was read as success: a stream that stopped
//! between fragments was filed as a `Stop`, a payload the decoder could not read was filed as an
//! unreachable provider, and an answer with nothing in it was filed as a turn that completed.
//!
//! What the kernel owes in all three cases is the same: say what happened, keep what was paid for,
//! and do not pretend the answer is one the caller can use.

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
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn capability() -> nucleos::contract::Capability {
    nucleos::contract::Capability {
        inputs: Vec::new(),
        outputs: Vec::new(),
        tool_calling: false,
        tool_choice_required: false,
        structured_output: false,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: None,
    }
}

/// A model that replays one script per call, so the *ending* is the only variable.
struct Scripted {
    /// One entry per call. An exhausted script repeats the last one, which is what a model that
    /// keeps failing that way actually does.
    scripts: std::sync::Mutex<Vec<Vec<ModelDelta>>>,
    calls: Arc<AtomicUsize>,
}

impl Scripted {
    fn once(deltas: Vec<ModelDelta>, calls: Arc<AtomicUsize>) -> Arc<dyn Model> {
        Self::sequence(vec![deltas], calls)
    }

    fn sequence(scripts: Vec<Vec<ModelDelta>>, calls: Arc<AtomicUsize>) -> Arc<dyn Model> {
        Arc::new(Self {
            scripts: std::sync::Mutex::new(scripts),
            calls,
        })
    }
}

#[async_trait]
impl Model for Scripted {
    fn capability(&self) -> nucleos::contract::Capability {
        capability()
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(
        &self,
        _prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<ModelStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let deltas = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.len() > 1 {
                scripts.remove(0)
            } else {
                scripts.first().cloned().unwrap_or_default()
            }
        };
        Ok(Box::pin(tokio_stream::iter(deltas.into_iter().map(Ok))))
    }
}

fn text(fragment: &str) -> ModelDelta {
    ModelDelta::PartAppend {
        part_index: 0,
        part: nucleos::contract::Part::text(fragment.to_owned()),
    }
}

/// A kernel, and the store the log can be read back from.
fn kernel(model: Arc<dyn Model>, retries: u32) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", model)
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries,
        })
        .build()
        .unwrap();
    (kernel, store)
}

/// A stream that ends between fragments failed, and saying `Stop` for it would be an invention.
#[tokio::test]
async fn a_stream_that_ends_without_a_finish_reason_fails_the_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    // One fragment, then nothing: no `Finished`, which is what a dropped connection looks like.
    let model = Scripted::once(vec![text("half an ans")], Arc::clone(&calls));
    // Retries are offered and must not be used: an attempt that produced content is never retried,
    // because two partial answers in one turn is worse than one.
    let (kernel, store) = kernel(model, 1);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ProviderTruncated);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a truncated answer was retried"
    );

    // The fragment was paid for and rendered, so it stays; the turn's ending says it is not whole.
    let events = store.events_since(&session.key(), 0).await.unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            EventKind::PartAppend { part, .. }
                if matches!(&part.kind, nucleos::contract::PartKind::Text(text) if text.text == "half an ans")
        )),
        "the partial answer was dropped from the log"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::MessageCompleted { .. })),
        "a truncated answer was recorded as a completed message"
    );
}

/// A truncated attempt that owes nothing is retried, like any other attempt with no content.
///
/// The distinction matters: "never retry a truncated stream" would throw away the case where the
/// connection died before the model said anything, which is the case a retry actually fixes.
#[tokio::test]
async fn a_truncated_stream_that_produced_nothing_is_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    // No deltas at all: the stream opens and closes.
    let model = Scripted::once(Vec::new(), Arc::clone(&calls));
    let (kernel, store) = kernel(model, 1);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ProviderTruncated);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an attempt with no content was not retried"
    );

    // Both attempts are in the log: a retry does not erase the attempt that failed.
    let attempts = store
        .events_since(&session.key(), 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.kind, EventKind::AttemptFailed { .. }))
        .count();
    assert_eq!(attempts, 2, "an attempt went unrecorded");
}

/// A model that finishes and produces nothing has not answered.
#[tokio::test]
async fn an_empty_answer_is_not_a_successful_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Scripted::once(
        vec![ModelDelta::Finished {
            reason: FinishReason::Stop,
        }],
        Arc::clone(&calls),
    );
    let (kernel, store) = kernel(model, 0);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ProviderResponseUnusable);

    let events = store.events_since(&session.key(), 0).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::MessageCompleted { .. })),
        "an empty answer entered history"
    );
}

/// A structurally broken stream fails the turn instead of assembling something the model did not say.
///
/// The case this replaces: a call whose arguments arrived without an announcement used to reach the
/// assembler, match no part index, and be dropped — a turn that succeeded with a tool call deleted
/// from its own history.
#[tokio::test]
async fn arguments_without_an_announcement_fail_the_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Scripted::once(
        vec![
            text("thinking"),
            ModelDelta::ToolCallArgs {
                index: 0,
                fragment: "{\"path\":\"a\"}".to_owned(),
            },
            ModelDelta::Finished {
                reason: FinishReason::ToolCalls,
            },
        ],
        Arc::clone(&calls),
    );
    let (kernel, store) = kernel(model, 0);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::AdapterContractViolation);

    let events = store.events_since(&session.key(), 0).await.unwrap();
    // What had already been rendered stays; the turn still has no completed message.
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::PartAppend { part, .. }
            if matches!(&part.kind, nucleos::contract::PartKind::Text(text) if text.text == "thinking")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::MessageCompleted { .. })),
        "a structurally broken stream produced a completed message"
    );
}

/// An empty answer is retried when the attempt owes nothing, and the retry can succeed.
///
/// The rule is the same one that governs every attempt: never after content, always otherwise. An
/// empty turn produced nothing, so a second attempt is the case a retry actually fixes — and the
/// first attempt is still recorded rather than erased by the second.
#[tokio::test]
async fn an_empty_answer_that_produced_nothing_is_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Scripted::sequence(
        vec![
            vec![ModelDelta::Finished {
                reason: FinishReason::Stop,
            }],
            vec![
                text("second time"),
                ModelDelta::Finished {
                    reason: FinishReason::Stop,
                },
            ],
        ],
        Arc::clone(&calls),
    );
    let (kernel, store) = kernel(model, 1);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let outcome = kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(outcome.message.content.text(), "second time");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the empty attempt was not retried"
    );

    let events = store.events_since(&session.key(), 0).await.unwrap();
    let failed = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::AttemptFailed { .. }))
        .count();
    assert_eq!(failed, 1, "the empty attempt left no record");
    let completed = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::MessageCompleted { .. }))
        .count();
    assert_eq!(
        completed, 1,
        "the recovered turn recorded the wrong number of messages"
    );
}

/// Two empty answers exhaust the retry budget, and both attempts are in the log.
#[tokio::test]
async fn an_empty_answer_is_retried_only_as_often_as_the_budget_allows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Scripted::sequence(
        vec![vec![ModelDelta::Finished {
            reason: FinishReason::Stop,
        }]],
        Arc::clone(&calls),
    );
    let (kernel, store) = kernel(model, 1);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::ProviderResponseUnusable);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the retry budget was not used"
    );

    let failed = store
        .events_since(&session.key(), 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.kind, EventKind::AttemptFailed { .. }))
        .count();
    assert_eq!(failed, 2, "an attempt went unrecorded");
}
