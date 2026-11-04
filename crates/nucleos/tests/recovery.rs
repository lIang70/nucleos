//! Reloading a session that died mid-turn.
//!
//! A checkpoint is a cache; the log is the truth. These tests write directly to the store to produce
//! what a crashed runtime leaves behind — events after the last checkpoint — and then ask `load` what
//! the session actually is. Writing the events by hand is the point: it is the shape of the failure,
//! and a real crash is neither cheap nor deterministic to produce.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{
    AttemptOutcome, ConfigPatch, Content, ErrorCode, Event, EventKind, Message, Part, PartKind,
    Principal, Role, Routing, SessionId, TextPart, Usage,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

/// The store's key for a test tenant. The tests all use one tenant, so the helper keeps them about
/// what they are testing rather than about spelling out a key at every call.
fn key(id: &SessionId) -> nucleos::adapter::SessionKey {
    nucleos::adapter::SessionKey::new("acme", id.clone())
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn store() -> Arc<SqliteStore> {
    Arc::new(SqliteStore::in_memory().unwrap())
}

fn kernel(store: &Arc<SqliteStore>) -> Kernel {
    Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: Some(10_000),
        ..SessionSpec::default()
    }
}

/// Append what a crashed runtime would have left: events continuing the log after its last
/// checkpoint.
async fn append_tail(store: &Arc<SqliteStore>, id: &SessionId, kinds: Vec<EventKind>) {
    let head = store
        .events_since(&key(id), 0)
        .await
        .unwrap()
        .last()
        .map(|event| event.seq + 1)
        .unwrap_or(0);
    let events: Vec<Event> = kinds
        .into_iter()
        .enumerate()
        .map(|(offset, kind)| Event {
            seq: head + offset as u64,
            session: id.clone(),
            at_unix_millis: 1,
            schema_version: Event::SCHEMA_VERSION,
            kind,
            meta: Default::default(),
        })
        .collect();
    store.append(&key(id), &events).await.unwrap();
}

#[tokio::test]
async fn spend_written_after_the_checkpoint_is_not_forgotten() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    let session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let before = session.usage().total_tokens();

    // A partial attempt that streamed and then the process died: the provider billed it.
    append_tail(
        &store,
        &id,
        vec![EventKind::Usage {
            usage: Usage {
                input_tokens: 700,
                output_tokens: 300,
                micros_usd: Some(1_500),
            },
            purpose: nucleos::contract::Purpose::Answer,
            model: "primary".to_owned(),
            attempt_index: 0,
        }],
    )
    .await;

    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.usage().total_tokens(), before + 1_000);
    assert!(
        loaded.usage().total_tokens() < 10_000,
        "the credit was spent"
    );
    assert_eq!(
        loaded.ledger().attempts.len(),
        1,
        "the bill is reconstructed, not just the total"
    );
}

#[tokio::test]
async fn one_attempt_that_reported_usage_three_times_is_one_entry_in_the_bill() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // A stream reports usage as it goes. Three reports, one attempt: a bill, not three.
    let report = |index: u32| EventKind::Usage {
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            micros_usd: Some(10),
        },
        purpose: nucleos::contract::Purpose::Answer,
        model: "primary".to_owned(),
        attempt_index: index,
    };
    append_tail(&store, &id, vec![report(0), report(0), report(0)]).await;

    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.ledger().attempts.len(), 1, "one attempt");
    assert_eq!(
        loaded.ledger().attempts[0].usage.total_tokens(),
        450,
        "every report is charged"
    );
    assert_eq!(loaded.usage().total_tokens(), 450);
}

#[tokio::test]
async fn a_message_that_completed_after_the_checkpoint_is_restored() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let message = Message {
        role: Role::Assistant,
        content: Content::new([Part::new(PartKind::Text(TextPart {
            text: "the answer".to_owned(),
        }))]),
    };
    append_tail(
        &store,
        &id,
        vec![EventKind::MessageCompleted {
            message: message.clone(),
            step_index: 0,
        }],
    )
    .await;

    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.messages(), [message]);
}

#[tokio::test]
async fn a_configuration_change_after_the_checkpoint_is_restored() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    append_tail(
        &store,
        &id,
        vec![EventKind::SessionConfigured {
            patch: ConfigPatch {
                system: Some(Some("be brief".to_owned())),
                ..ConfigPatch::default()
            },
        }],
    )
    .await;

    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.system(), Some("be brief"));
}

#[tokio::test]
async fn a_turn_the_runtime_did_not_survive_is_recorded_rather_than_inferred() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Text streamed, a tool call started, nothing completed: the shape of a crash mid-turn.
    append_tail(
        &store,
        &id,
        vec![
            EventKind::TextDelta {
                part_index: 0,
                text: "Let me look at".to_owned(),
            },
            EventKind::ToolCallStarted {
                index: 0,
                tool_call_id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                deadline_unix_millis: 1_000,
                progress_token: "t1".to_owned(),
            },
        ],
    )
    .await;

    let loaded = kernel.load(&principal(), &id).await.unwrap();

    // The session does not pretend the message completed: a fragment the model never finished
    // writing is not history it should reason from.
    assert!(loaded.messages().is_empty(), "{:?}", loaded.messages());
    assert!(!loaded.is_closed(), "the session is still usable");

    // And the log now says what happened, in the same words the ledger uses for a failed attempt.
    let failed = store
        .events_since(&key(&id), 0)
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::AttemptFailed { attempt } => Some(attempt),
            _ => None,
        })
        .expect("the abandoned turn is recorded");
    assert!(matches!(
        failed.outcome,
        AttemptOutcome::Failed {
            code: ErrorCode::Unavailable,
            ..
        }
    ));
    assert!(
        matches!(&failed.outcome, AttemptOutcome::Failed { reason, .. } if reason.contains("mid-turn")),
        "{:?}",
        failed.outcome
    );

    // Loading again does not append a second record: one crash is one fact.
    let again = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(again.messages(), loaded.messages());
    let records = store
        .events_since(&key(&id), 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.kind, EventKind::AttemptFailed { .. }))
        .count();
    assert_eq!(
        records, 1,
        "the second load found no evidence left to infer"
    );
}

#[tokio::test]
async fn a_tail_of_narrative_events_is_not_a_crash() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // A degraded parameter and a progress tick: both describe what a consumer saw. Neither is a turn
    // that died, and treating them as one would mark every session that ever streamed.
    append_tail(
        &store,
        &id,
        vec![
            EventKind::CapabilityDegraded(nucleos::contract::Degraded {
                field: "tool_choice".to_owned(),
                reason: "the provider ignored it".to_owned(),
            }),
            EventKind::Progress {
                token: "t1".to_owned(),
                done: 1,
                total: 10,
                message: "working".to_owned(),
            },
        ],
    )
    .await;

    kernel.load(&principal(), &id).await.unwrap();
    assert!(
        !store
            .events_since(&key(&id), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, EventKind::AttemptFailed { .. })),
        "no turn was abandoned"
    );
}

// ---------------------------------------------------------------------------
// The fragment, reported rather than left to be reassembled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_turn_that_died_mid_generation_reports_what_it_had_said() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    append_tail(
        &store,
        &id,
        vec![
            EventKind::MessageCompleted {
                message: Message {
                    role: Role::Assistant,
                    content: Content::new([Part::new(PartKind::Text(TextPart {
                        text: "an earlier, finished answer".to_owned(),
                    }))]),
                },
                step_index: 0,
            },
            EventKind::TextDelta {
                part_index: 0,
                text: "check the config, then run ".to_owned(),
            },
            EventKind::TextDelta {
                part_index: 0,
                text: "migrate".to_owned(),
            },
            EventKind::ToolCallStarted {
                index: 0,
                tool_call_id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                deadline_unix_millis: 1_000,
                progress_token: "t1".to_owned(),
            },
            EventKind::ToolCallArgs {
                index: 0,
                fragment: "{\"path\": \"/etc/nuc".to_owned(),
            },
        ],
    )
    .await;

    let session = kernel.load(&principal(), &id).await.unwrap();
    let partial = kernel
        .partial(&session)
        .await
        .unwrap()
        .expect("the turn was cut short");

    // Only the unfinished turn: the completed answer before it is history, not a fragment.
    assert_eq!(partial.text, "check the config, then run migrate");
    let call = partial.unfinished_call.expect("it stopped inside a call");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.tool_call_id, "call-1");
    // Reported as it arrived, truncation and all: it is data for a caller to look at, never something
    // the kernel executes.
    assert_eq!(call.arguments_json, "{\"path\": \"/etc/nuc");
}

#[tokio::test]
async fn a_finished_turn_has_no_partial() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    append_tail(
        &store,
        &id,
        vec![
            EventKind::TextDelta {
                part_index: 0,
                text: "the whole answer".to_owned(),
            },
            EventKind::MessageCompleted {
                message: Message {
                    role: Role::Assistant,
                    content: Content::new([Part::new(PartKind::Text(TextPart {
                        text: "the whole answer".to_owned(),
                    }))]),
                },
                step_index: 0,
            },
        ],
    )
    .await;

    let session = kernel.load(&principal(), &id).await.unwrap();
    assert!(
        kernel.partial(&session).await.unwrap().is_none(),
        "a completed turn leaves no fragment"
    );
}

#[tokio::test]
async fn a_session_that_never_ran_has_no_partial() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    let session = kernel.open(&principal(), id, spec()).await.unwrap();
    assert!(kernel.partial(&session).await.unwrap().is_none());
}

/// A turn that ended by writing down why is not an abandoned turn.
///
/// `turn_failed` exists so a failure that never became an attempt still has a record. It also has to
/// *end* the turn for recovery: `fold` treats `turn_started` as evidence of an abandoned turn, and
/// `completes_a_unit` is what retires that evidence. Without it, `load` appends `attempt_failed`
/// beneath a turn that already said what happened — inventing a model, a trigger and a duration.
#[tokio::test]
async fn a_turn_that_ended_in_writing_is_not_re_load_ed_as_abandoned() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        // Denies the model, so the turn fails inside the call — the path that writes `turn_failed`
        // and nothing else.
        .policy(Arc::new(nucleos::adapter::DenyAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();

    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(1_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        kernel
            .inference(&mut session, &[])
            .await
            .unwrap_err()
            .code(),
        ErrorCode::Denied
    );

    let before = kernel.events(&principal(), &id, 0).await.unwrap();
    assert!(
        matches!(
            before.last().map(|event| &event.kind),
            Some(EventKind::TurnFailed { .. })
        ),
        "the turn ended in writing: {:?}",
        before
            .iter()
            .map(|event| event.kind.type_name())
            .collect::<Vec<_>>()
    );

    // Loading is what used to append a second, invented ending.
    kernel.load(&principal(), &id).await.unwrap();
    let after = kernel.events(&principal(), &id, 0).await.unwrap();
    assert_eq!(
        after.len(),
        before.len(),
        "a load added an event to a turn that had already ended: {:?}",
        after
            .iter()
            .map(|event| event.kind.type_name())
            .collect::<Vec<_>>()
    );
}

/// A turn that ends inside the checkpoint window is not checkpointed, and must still reload as the
/// same session.
///
/// This is what the throttle buys and what it may not cost: the log holds the turn, so a reload folds
/// it — the message, the step it charged to the budget, and the head a view reads.
#[tokio::test]
async fn a_turn_that_ends_without_a_checkpoint_reloads_as_the_same_session() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let outcome = kernel.inference(&mut session, &[]).await.unwrap();

    // The point of the test: the checkpoint really is behind the log, so everything below is folded
    // rather than read from a snapshot.
    let stored = store.snapshot(&key(&id)).await.unwrap().unwrap();
    assert!(
        stored.seq + 1 < session.next_seq(),
        "the checkpoint is at {} and the log at {}: the throttle did not fire",
        stored.seq,
        session.next_seq()
    );

    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.messages(), [outcome.message]);
    assert_eq!(
        loaded.inference_calls(),
        session.inference_calls(),
        "a completed turn is a step the budget charged, and a reload does not give it back"
    );

    // The view path: the same session, without parsing the conversation.
    let head = kernel.load_head(&principal(), &id).await.unwrap();
    assert!(head.messages().is_empty(), "a head carries no conversation");
    assert_eq!(head.inference_calls(), loaded.inference_calls());
    assert_eq!(head.is_closed(), loaded.is_closed());
    assert_eq!(head.next_seq(), loaded.next_seq());

    // Idempotent: the first load wrote the recovery record, and a second one must not add to it.
    let again = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(again.messages().len(), 1);
    assert_eq!(again.inference_calls(), loaded.inference_calls());
}
