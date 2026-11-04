//! A call parked on a human, and what running it does to the park.
//!
//! The parked entry is the kernel's record of "this has not run yet". It has to be consumed by the
//! answer, or two things follow: the session reports a question that was already answered for the rest
//! of its life, and a retried verdict — the client never saw the first response, so it sends the same
//! one again — finds the entry and runs the tool a second time. Tools are not idempotent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{
    PendingCall, PermissionKind, PermissionOption, Principal, Routing, SessionId, ToolCall,
    ToolSpec,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".to_owned(),
            description: "echoes".to_owned(),
            input_schema_json: r#"{"type":"object","properties":{"text":{}}}"#.to_owned(),
            isolation: Some(nucleos::contract::Isolation::InProcess),
        }
    }

    async fn invoke(&self, args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        ToolOutput::text(args)
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .tool(Arc::new(Echo))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (kernel, store)
}

fn call() -> ToolCall {
    ToolCall {
        id: "call-1".to_owned(),
        name: "echo".to_owned(),
        arguments_json: r#"{"text":"hi"}"#.to_owned(),
        meta: std::collections::BTreeMap::new(),
    }
}

fn pending() -> PendingCall {
    PendingCall {
        call: call(),
        options: vec![PermissionOption {
            id: "once".to_owned(),
            kind: PermissionKind::AllowOnce,
            label: "Allow once".to_owned(),
        }],
        question: "Run echo?".to_owned(),
    }
}

#[tokio::test]
async fn answering_a_parked_call_removes_it() {
    let (kernel, _) = kernel();
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: 1_000,
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    kernel.park(&mut session, pending()).await.unwrap();
    assert_eq!(session.pending().len(), 1);

    kernel
        .invoke_approved(&mut session, &[call()])
        .await
        .expect("the answer runs the call");
    assert!(
        session.pending().is_empty(),
        "a call that ran is not waiting any more"
    );

    // And the removal is durable: a reload must not resurrect the question.
    let reloaded = kernel.load(&principal(), &id).await.unwrap();
    assert!(
        reloaded.pending().is_empty(),
        "the checkpoint still has the parked call"
    );
}

#[tokio::test]
async fn a_retried_verdict_finds_nothing_to_answer() {
    // The client never saw the first response, so it sends the same verdict again. The answer path
    // looks the call up in `pending` before doing anything; with the entry consumed, the retry is
    // refused instead of running a non-idempotent tool a second time.
    let (kernel, store) = kernel();
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: 1_000,
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    kernel.park(&mut session, pending()).await.unwrap();
    kernel
        .invoke_approved(&mut session, &[call()])
        .await
        .unwrap();

    // A second answer arrives after a reload, which is what a retried request looks like.
    let reloaded = kernel.load(&principal(), &id).await.unwrap();
    assert!(
        reloaded.pending().is_empty(),
        "a retried verdict must find nothing to answer, not a call to run again"
    );

    let finished = store
        .events_since(&nucleos::adapter::SessionKey::new("acme", id), 0)
        .await
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                nucleos::contract::EventKind::ToolFinished { .. }
            )
        })
        .count();
    assert_eq!(finished, 1, "the tool ran once");
}
