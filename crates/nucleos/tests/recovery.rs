//! Reloading a session that died mid-turn.
//!
//! A checkpoint is a cache; the log is the truth. These tests write directly to the store to produce
//! what a crashed runtime leaves behind — events after the last checkpoint — and then ask `load` what
//! the session actually is. Writing the events by hand is the point: it is the shape of the failure,
//! and a real crash is neither cheap nor deterministic to produce.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
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
/// Denies the model and allows everything else: a session that opens and a turn that fails.
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
async fn a_turn_that_ended_in_writing_is_not_re_load_ed_as_abandoned() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        // Denies the model and nothing else, so the turn fails inside the call — the path that
        // writes `turn_failed` and nothing else. `DenyAll` would refuse the session at `open`, since
        // the isolation a caller asks for is gated there too.
        .policy(Arc::new(DenyModelOnly))
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

/// Two handles on one session: the one that checkpoints later must not claim events it never applied.
///
/// The log is shared, so a second handle can append while the first is still working. A checkpoint the
/// first handle writes must say how far *it* got, not where the log's head is — claiming the head would
/// tell the next `load` that the second handle's events were already folded in, and they would be
/// skipped for good. That is what the throttled turn path made routine.
#[tokio::test]
async fn a_checkpoint_does_not_claim_events_another_handle_wrote() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");

    // Two handles, loaded before either writes.
    let mut first = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let mut second = kernel.load(&principal(), &id).await.unwrap();

    // The second handle finishes a turn: its message lands in the log, and it is checkpointed.
    kernel.inference(&mut second, &[]).await.unwrap();
    let answered = second.next_seq();
    kernel.checkpoint(&mut second).await.unwrap();

    // The first handle writes a checkpoint of its own, having applied nothing since it loaded.
    kernel.checkpoint(&mut first).await.unwrap();

    // Whatever the store now claims, a reload must still see the second handle's message.
    let reloaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(
        reloaded.messages().len(),
        1,
        "the message the second handle wrote was skipped: the first handle's checkpoint claimed it"
    );
    assert!(
        reloaded.next_seq() >= answered,
        "the log head is at least where the second handle left it"
    );
}

/// A refused durable checkpoint is reported, not swallowed.
///
/// `append` is the only write with no event behind it: its checkpoint *is* its durability. Treating a
/// stale-version refusal as success told the caller `204` for a write that was in neither the log nor
/// the snapshot.
#[tokio::test]
async fn an_append_that_cannot_be_checkpointed_is_an_error() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");

    let mut first = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let mut second = kernel.load(&principal(), &id).await.unwrap();

    // The second handle advances the store's version.
    kernel.inference(&mut second, &[]).await.unwrap();
    kernel.checkpoint(&mut second).await.unwrap();

    // The first handle's append now loses the version race. Its content exists nowhere else, so the
    // answer is an error rather than a cheerful no-op.
    let refused = kernel
        .append(
            &mut first,
            nucleos::kernel::StateWrite {
                messages: Vec::new(),
                data: [("k".to_owned(), serde_json::json!("v"))]
                    .into_iter()
                    .collect(),
            },
        )
        .await;
    assert!(
        refused.is_err(),
        "a durable checkpoint that was refused must not report success"
    );
}

/// Opening an id that already exists is a conflict, not a fresh empty session over a live log.
#[tokio::test]
async fn opening_an_existing_session_is_refused_with_an_instruction() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let before = kernel.events(&principal(), &id, 0).await.unwrap().len();
    let error = kernel
        .open(&principal(), id.clone(), spec())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Conflict);
    assert!(error.reason().contains("resume"), "{error}");
    assert_eq!(
        kernel.events(&principal(), &id, 0).await.unwrap().len(),
        before,
        "a refused open wrote to the existing log"
    );

    // Closed is a different instruction: this id is used, not resumable.
    let mut session = kernel.load(&principal(), &id).await.unwrap();
    kernel
        .close(&mut session, nucleos::contract::StopReason::Completed)
        .await
        .unwrap();
    let error = kernel
        .open(&principal(), id.clone(), spec())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Conflict);
    assert!(error.reason().contains("closed"), "{error}");

    // Another subject's id is answered as absent, so opening cannot be used to enumerate ids.
    let other = Principal {
        tenant: "acme".to_owned(),
        subject: "bob".to_owned(),
    };
    let error = kernel.open(&other, id, spec()).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
}

/// A checkpoint hands the store its tail, not the conversation.
///
/// The store has always written only the rows it did not hold; the session used to clone the whole
/// conversation to tell it so. What a checkpoint carries is now the delta, which is what makes the
/// cost of a checkpoint proportional to what changed rather than to the session's age.
#[tokio::test]
async fn a_checkpoint_carries_the_tail_and_the_store_counts_the_whole() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Three turns, each appending one message through `state` — the only write with no event behind it.
    for turn in 0..3u64 {
        let mut session = kernel.load(&principal(), &id).await.unwrap();
        let outcome = kernel.inference(&mut session, &[]).await.unwrap();
        kernel
            .append(
                &mut session,
                nucleos::kernel::StateWrite {
                    messages: vec![outcome.message],
                    ..nucleos::kernel::StateWrite::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            session.messages().len() as u64,
            turn + 1,
            "the live handle keeps the conversation"
        );
    }

    // The store holds three rows, and the session read them back rather than re-sending them.
    let stored = store.snapshot(&key(&id)).await.unwrap().unwrap();
    assert_eq!(stored.messages.len(), 3);

    let mut session = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(session.messages().len(), 3);
    // A checkpoint taken now owes the store nothing, and says so by carrying an empty tail.
    kernel.checkpoint(&mut session).await.unwrap();
    assert_eq!(
        store
            .snapshot(&key(&id))
            .await
            .unwrap()
            .unwrap()
            .messages
            .len(),
        3,
        "a checkpoint with no work rewrote or duplicated rows"
    );
}

/// A tail longer than one request should carry is folded in pages, and its progress is recorded.
///
/// The fold used to read the whole tail into one vector. It still folds everything — the log is the
/// truth and refusing would lose work — but the memory is one page, and past a budget the folded state
/// is checkpointed so the next request starts closer instead of repeating it.
#[tokio::test]
async fn a_long_tail_is_folded_in_pages_and_its_progress_recorded() {
    let store = store();
    let kernel = kernel(&store);
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let before = store.snapshot(&key(&id)).await.unwrap().unwrap().seq;

    // More than one budget's worth of narrative events: an ordinary production tail is a few hundred,
    // and this is what a checkpoint that fell far behind leaves behind.
    let start = kernel.load(&principal(), &id).await.unwrap().next_seq();
    let mut batch = Vec::new();
    for index in 0..9_000u64 {
        batch.push(Event {
            seq: start + index,
            session: id.clone(),
            at_unix_millis: index,
            schema_version: Event::SCHEMA_VERSION,
            kind: EventKind::TextDelta {
                part_index: 0,
                text: "x".to_owned(),
            },
            meta: Default::default(),
        });
        if batch.len() == 512 {
            store.append(&key(&id), &batch).await.unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.append(&key(&id), &batch).await.unwrap();
    }

    // The first load folds all of it, and checkpoints what it had folded when it hit the budget.
    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.next_seq(), start + 9_000);
    let after = store.snapshot(&key(&id)).await.unwrap().unwrap();
    assert!(
        after.seq > before,
        "the load folded twice the budget and recorded nothing: {} is still {}",
        after.seq,
        before
    );
    assert!(
        after.seq < start + 8_999,
        "the progress checkpoint should be mid-tail, not the whole thing: {}",
        after.seq
    );

    // A second load picks up from there and has less than a budget left, so it records nothing new.
    let again = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(again.next_seq(), start + 9_000);
    assert_eq!(
        store.snapshot(&key(&id)).await.unwrap().unwrap().seq,
        after.seq,
        "a load with nothing to record wrote a checkpoint anyway"
    );
}
