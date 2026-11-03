//! Measurements, not tests.
//!
//!     cargo test --release --test bench -- --ignored --nocapture
//!
//! The numbers that matter are ratios between runs, because the cost model is what is being fixed:
//! a read that grows with the session's age, and a write that pays a transaction per streamed delta.
//! Absolute milliseconds depend on the machine and say nothing on their own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{
    Capability, Event, EventKind, Principal, Routing, SessionId, SessionState,
};
use nucleos::kernel::{Kernel, SessionSpec};
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
                credit_tokens: 10_000_000,
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
                credit_tokens: 10_000_000,
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
