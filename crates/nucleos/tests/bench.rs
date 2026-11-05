//! Measurements, not tests.
//!
//!     cargo test --release --test bench -- --ignored --nocapture
//!
//! The numbers that matter are ratios between runs, because the cost model is what is being fixed:
//! a read that grows with the session's age, a write whose cost follows the conversation's length, and
//! a view that should not touch either. Absolute milliseconds depend on the machine and say nothing on
//! their own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{
    Capability, Content, Event, EventKind, Message, Part, PartKind, Principal, Role, Routing,
    SessionId, SessionState, TextPart,
};
use nucleos::kernel::{Kernel, SessionSpec, StateWrite};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;
use std::time::Instant;

/// A model that streams `chunks` pieces of text and stops.
struct Streamer {
    chunks: usize,
}

#[async_trait]
impl Model for Streamer {
    fn capability(&self) -> Capability {
        Capability {
            tools: true,
            tool_choice_required: true,
            structured_output: true,
            streaming: true,
            context_window: 1_000_000,
            max_output_tokens: 100_000,
        }
    }
    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }
    async fn invoke(&self, _request: ModelRequest) -> nucleos::error::Result<ModelStream> {
        let mut deltas: Vec<nucleos::error::Result<ModelDelta>> = (0..self.chunks)
            .map(|index| {
                Ok(ModelDelta::Text {
                    part_index: 0,
                    text: format!("{index} "),
                })
            })
            .collect();
        deltas.push(Ok(ModelDelta::Finished {
            reason: FinishReason::Stop,
        }));
        Ok(Box::pin(tokio_stream::iter(deltas)))
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel(chunks: usize) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(Streamer { chunks }))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (kernel, store)
}

fn limit() -> usize {
    std::env::var("BENCH_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
}

/// One turn that streams N deltas: the write path, end to end.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn write_a_turn_of_deltas() {
    let chunks = limit();
    let (kernel, store) = kernel(chunks);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(10_000_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    let opened = Instant::now();
    let outcome = kernel.inference(&mut session, &[]).await.unwrap();
    let elapsed = opened.elapsed();
    let events = store
        .events_since(&nucleos::adapter::SessionKey::new("acme", id), 0)
        .await
        .unwrap()
        .len();
    println!(
        "write: {chunks} deltas → {} events in {:?} ({:.1} µs/event)",
        events,
        elapsed,
        elapsed.as_micros() as f64 / events as f64
    );
    assert_eq!(outcome.message.content.parts.len(), 1);
}

/// `GET /v1/sessions/{id}` on a session with a long history.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn read_the_partial_of_a_long_session() {
    let history = limit() * 50;
    let (kernel, store) = kernel(0);
    let id = SessionId::new("s1");
    kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(10_000_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // A session old enough that "read everything" is the wrong cost model.
    let key = nucleos::adapter::SessionKey::new("acme", id.clone());
    let head = kernel.load(&principal(), &id).await.unwrap().next_seq();
    let mut batch = Vec::new();
    for index in 0..history {
        batch.push(Event {
            seq: head + index as u64,
            session: id.clone(),
            at_unix_millis: index as u64,
            schema_version: Event::SCHEMA_VERSION,
            kind: EventKind::ReasoningDelta {
                part_index: 0,
                text: "thinking ".to_owned(),
                signature: String::new(),
            },
            meta: Default::default(),
        });
        if batch.len() == 1024 {
            store.append(&key, &batch).await.unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.append(&key, &batch).await.unwrap();
    }

    let session = kernel.load(&principal(), &id).await.unwrap();
    let started = Instant::now();
    let partial = kernel.partial(&session).await.unwrap();
    println!(
        "read: partial over {history} historical events → {:?} in {:?}",
        partial.as_ref().map(|partial| partial.text.len()),
        started.elapsed()
    );

    let started = Instant::now();
    let all = kernel.events(&principal(), &id, 0).await.unwrap().len();
    println!(
        "read: events(from_seq=0) → {all} events in {:?}",
        started.elapsed()
    );
}

/// The store on its own: one transaction per event, which is what the kernel does today.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn write_one_transaction_per_event() {
    let count = limit() * 5;
    let store = SqliteStore::in_memory().unwrap();
    let key = nucleos::adapter::SessionKey::new("acme", SessionId::new("s1"));
    let started = Instant::now();
    for index in 0..count {
        store
            .append(
                &key,
                &[Event {
                    seq: index as u64,
                    session: SessionId::new("s1"),
                    at_unix_millis: 0,
                    schema_version: Event::SCHEMA_VERSION,
                    kind: EventKind::StateChanged {
                        state: SessionState::Open,
                        stop: None,
                    },
                    meta: Default::default(),
                }],
            )
            .await
            .unwrap();
    }
    let elapsed = started.elapsed();
    println!(
        "store: {count} single-event transactions in {:?} ({:.1} µs/event)",
        elapsed,
        elapsed.as_micros() as f64 / count as f64
    );

    // And the same events in batches, which is what coalescing would do.
    let store = SqliteStore::in_memory().unwrap();
    let started = Instant::now();
    let mut batch = Vec::new();
    for index in 0..count {
        batch.push(Event {
            seq: index as u64,
            session: SessionId::new("s1"),
            at_unix_millis: 0,
            schema_version: Event::SCHEMA_VERSION,
            kind: EventKind::StateChanged {
                state: SessionState::Open,
                stop: None,
            },
            meta: Default::default(),
        });
        if batch.len() == 16 {
            store.append(&key, &batch).await.unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.append(&key, &batch).await.unwrap();
    }
    let elapsed = started.elapsed();
    println!(
        "store: {count} events in batches of 16 in {:?} ({:.1} µs/event)",
        elapsed,
        elapsed.as_micros() as f64 / count as f64
    );
}

/// A turn near the start of a session, and one near the end of a long one.
///
/// The ratio is the property. With a checkpoint after every turn this grew tenfold by 1,200 messages,
/// because each turn rewrote the whole conversation into the snapshot. The throttle (`pitfalls` 105)
/// and the split — the conversation and the ledger are rows of their own — are what keep both flat:
/// a turn and its append cost the same on the first message as on the twelve-hundredth.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_costs_the_same_at_the_start_and_the_end() {
    let turns = limit().min(600);
    let (kernel, _store) = kernel(4);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id,
            SessionSpec {
                credit_tokens: Some(100_000_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    let answer = "x".repeat(500);
    let (mut first, mut last) = (Vec::new(), Vec::new());
    let (mut first_append, mut last_append) = (Vec::new(), Vec::new());
    for turn in 0..turns {
        let started = Instant::now();
        let outcome = kernel.inference(&mut session, &[]).await.unwrap();
        let turn_elapsed = started.elapsed();

        let started = Instant::now();
        kernel
            .append(
                &mut session,
                StateWrite {
                    messages: vec![
                        outcome.message,
                        Message {
                            role: Role::Assistant,
                            content: Content::new([Part::new(PartKind::Text(TextPart {
                                text: answer.clone(),
                            }))]),
                        },
                    ],
                    ..StateWrite::default()
                },
            )
            .await
            .unwrap();
        let append_elapsed = started.elapsed();

        if turn < 20 {
            first.push(turn_elapsed);
            first_append.push(append_elapsed);
        }
        if turn >= turns.saturating_sub(20) {
            last.push(turn_elapsed);
            last_append.push(append_elapsed);
        }
    }
    let avg = |values: &[std::time::Duration]| values.iter().sum::<std::time::Duration>() / 20;

    println!(
        "turn: first20 {:?}, last20 {:?}; append: first20 {:?}, last20 {:?} (messages={})",
        avg(&first),
        avg(&last),
        avg(&first_append),
        avg(&last_append),
        session.messages().len()
    );
    assert_eq!(session.messages().len(), turns * 2);
}

/// The two reads a client can ask for: the whole session, and the view that polls.
///
/// The checkpoint is written first, so both reads measure the snapshot rather than a tail a `load` had
/// to fold: the property is that a view does not parse the conversation.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn a_view_does_not_parse_the_conversation() {
    let history = limit();
    let (kernel, store) = kernel(0);
    let id = SessionId::new("s1");
    let session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(10_000_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    let key = nucleos::adapter::SessionKey::new("acme", id.clone());
    let head = session.next_seq();
    let text = "x".repeat(500);
    let mut batch = Vec::new();
    for index in 0..history {
        batch.push(Event {
            seq: head + index as u64,
            session: id.clone(),
            at_unix_millis: index as u64,
            schema_version: Event::SCHEMA_VERSION,
            kind: EventKind::MessageCompleted {
                message: Message {
                    role: Role::Assistant,
                    content: Content::new([Part::new(PartKind::Text(TextPart {
                        text: text.clone(),
                    }))]),
                },
                step_index: u32::try_from(index).unwrap_or(u32::MAX),
            },
            meta: Default::default(),
        });
        if batch.len() == 512 {
            store.append(&key, &batch).await.unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        store.append(&key, &batch).await.unwrap();
    }

    // Fold the history once and checkpoint it, so the reads below are about the snapshot.
    let mut folded = kernel.load(&principal(), &id).await.unwrap();
    kernel.checkpoint(&mut folded).await.unwrap();

    let started = Instant::now();
    let full = store.snapshot(&key).await.unwrap().unwrap();
    let snapshot = started.elapsed();
    let started = Instant::now();
    let head = store.snapshot_head(&key).await.unwrap().unwrap();
    let snapshot_head = started.elapsed();
    let started = Instant::now();
    let loaded = kernel.load(&principal(), &id).await.unwrap();
    let load = started.elapsed();
    let started = Instant::now();
    let view = kernel.load_head(&principal(), &id).await.unwrap();
    let load_head = started.elapsed();

    println!(
        "read: snapshot {:?} ({} messages, {} bytes), snapshot_head {:?} ({} messages, {} bytes)",
        snapshot,
        full.messages.len(),
        serde_json::to_string(&full).unwrap().len(),
        snapshot_head,
        head.messages.len(),
        serde_json::to_string(&head).unwrap().len()
    );
    println!(
        "read: load {:?} ({} messages), load_head {:?} ({} messages)",
        load,
        loaded.messages().len(),
        load_head,
        view.messages().len()
    );
    assert_eq!(full.messages.len(), history);
    assert!(head.messages.is_empty());
}
