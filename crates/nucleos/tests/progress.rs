//! Progress from a long tool call, as the ABI promises it.
//!
//! `EventKind::Progress` existed in the contract and nothing emitted it, so a call whose only
//! feedback was a deadline was indistinguishable from a hang. These tests drive a tool that reports
//! while it runs and ask the log what it saw.
//!
//! The shape that matters: a tool cannot write events. It sends to a sink, the kernel writes them
//! beside the session's other events, and a tool that says nothing produces no record at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Progress, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{EventKind, Principal, Routing, SessionId, ToolCall, ToolSpec};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

/// A tool that reports `steps` times while it runs.
struct Reporting {
    steps: u64,
}

#[async_trait]
impl Tool for Reporting {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow".to_owned(),
            description: "Takes a while, and says how far it has got.".to_owned(),
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
        }
    }

    async fn invoke(&self, _args: &str, cx: &ToolCtx<'_>) -> ToolOutput {
        for step in 0..self.steps {
            cx.progress.report(step + 1, self.steps, "working");
        }
        ToolOutput::text("done")
    }
}

fn kernel(steps: u64) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .tool(Arc::new(Reporting { steps }))
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

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: 10_000,
        ..SessionSpec::default()
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "call-1".to_owned(),
        name: "slow".to_owned(),
        arguments_json: "{}".to_owned(),
        meta: Default::default(),
    }
}

#[tokio::test]
async fn a_tool_that_reports_produces_one_event_per_step() {
    let (kernel, store) = kernel(3);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let output = kernel.invoke_tool(&mut session, &call()).await.unwrap();
    assert_eq!(output.content.text(), "done");

    let events = store.events_since(session.id(), 0).await.unwrap();
    let progress: Vec<(String, u64, u64, String)> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Progress {
                token,
                done,
                total,
                message,
            } => Some((token.clone(), *done, *total, message.clone())),
            _ => None,
        })
        .collect();

    // One record per step, in order, addressed by the call they belong to: a stream of "50%"
    // without the id is unattributable when a batch runs eight calls at once.
    assert_eq!(progress.len(), 3, "{progress:?}");
    assert!(progress
        .iter()
        .all(|(token, _, total, _)| token == "call-1" && *total == 3));
    assert_eq!(
        progress.iter().map(|(_, done, _, _)| *done).collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[tokio::test]
async fn a_tool_that_says_nothing_produces_no_record() {
    let (kernel, store) = kernel(0);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    kernel.invoke_tool(&mut session, &call()).await.unwrap();

    // Absence is the honest answer for a tool with nothing to report: a synthetic "0%" every call
    // would be noise pretending to be telemetry.
    assert!(
        !store
            .events_since(session.id(), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, EventKind::Progress { .. })),
        "no steps, no records"
    );
}

#[tokio::test]
async fn progress_shares_the_session_sequence_and_survives_a_reload() {
    let (kernel, store) = kernel(2);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    let id = session.id().clone();
    kernel.invoke_tool(&mut session, &call()).await.unwrap();

    // One writer, one counter: the events written beside the session must not leave a gap, or the
    // store refuses the next write and the session cannot continue.
    let seqs: Vec<u64> = store
        .events_since(&id, 0)
        .await
        .unwrap()
        .iter()
        .map(|event| event.seq)
        .collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());

    // Replaying them is harmless: progress is narrative, so a reloaded session is unchanged by it.
    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(loaded.next_seq(), seqs.len() as u64);
}

/// A tool that reports through a sink it does not own is the point; this pins the trait's shape.
#[test]
fn a_sink_can_be_discarded() {
    let sink: &dyn Progress = &nucleos::adapter::NoProgress;
    sink.report(1, 1, "nothing to do");
}
