//! What must fit, and who decides what to drop.
//!
//! The kernel owns one half of context management and refuses the other. It measures a prompt
//! against the model's window and refuses to send one that cannot fit, because a provider that
//! silently truncates changes the answer without saying so. It does **not** decide what to drop:
//! the caller assembled the message list, so replacing part of it is the caller's decision — and
//! many strategies exist with different failure modes. The one thing the caller must not do is
//! leave the log describing a prompt that was never sent, which is what `compact` records.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{
    Capability, ConfigPatch, ErrorCode, EventKind, GenerateParams, Principal, Routing, SessionId,
};
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

fn capability(window: u32, max_output: u32) -> Capability {
    Capability {
        tools: true,
        tool_choice_required: true,
        structured_output: true,
        streaming: true,
        context_window: window,
        max_output_tokens: max_output,
    }
}

/// A spec with room to spend: these tests are about the context window, and a session with no
/// credit would stop before the window was ever looked at.
fn funded() -> SessionSpec {
    SessionSpec {
        credit_tokens: 1_000_000,
        ..SessionSpec::default()
    }
}

fn build(model: FakeModel) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(model))
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

#[tokio::test]
async fn a_prompt_that_cannot_fit_is_refused_before_it_is_sent() {
    // A window of 1000, a completion reserve of 100, and a prompt the adapter counts as 5000.
    let model = FakeModel::text(["unused"])
        .with_capability(capability(1000, 100))
        .with_token_count(5000);
    let (kernel, _store) = build(model);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), funded())
        .await
        .unwrap();

    let refused = kernel.inference(&mut session, &[]).await;

    let error = refused.expect_err("a prompt over the window is not sent");
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("window"), "{error:?}");
}

#[tokio::test]
async fn the_completion_reserve_is_part_of_what_has_to_fit() {
    // 950 fits in 1000 on its own; 950 plus a 100-token reserve does not.
    let model = FakeModel::text(["ok"])
        .with_capability(capability(1000, 100))
        .with_token_count(950);
    let (kernel, _store) = build(model);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), funded())
        .await
        .unwrap();

    let refused = kernel.inference(&mut session, &[]).await;
    assert!(
        refused.is_err(),
        "950 fits in 1000, but not with a 100-token completion reserved"
    );

    // And with a smaller prompt it runs, so the test is about the reserve and not about the window.
    let model = FakeModel::text(["ok"])
        .with_capability(capability(1000, 100))
        .with_token_count(500);
    let (kernel, _store) = build(model);
    let mut session = kernel
        .open(&principal(), SessionId::new("s2"), funded())
        .await
        .unwrap();
    kernel.inference(&mut session, &[]).await.unwrap();
}

#[tokio::test]
async fn a_model_that_declares_no_window_is_not_treated_as_unlimited() {
    let model = FakeModel::text(["unused"])
        .with_capability(capability(0, 0))
        .with_token_count(10);
    let (kernel, _store) = build(model);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), funded())
        .await
        .unwrap();

    // "No declared limit" cannot mean "send anything": that is the silent truncation this check
    // exists to prevent, just moved one layer down.
    assert!(kernel.inference(&mut session, &[]).await.is_err());
}

#[tokio::test]
async fn the_model_declares_a_reserve_and_the_request_may_lower_it() {
    let model = FakeModel::text(["ok"])
        .with_capability(capability(1000, 400))
        .with_token_count(700);
    let (kernel, _store) = build(model);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), funded())
        .await
        .unwrap();

    // 700 + 400 is over a window of 1000, with the reserve taken from the model's own declaration.
    assert!(kernel.inference(&mut session, &[]).await.is_err());

    // Asking for a smaller completion is the cheapest way to make the same prompt fit, and it is
    // the request's number that decides.
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                params: Some(GenerateParams {
                    max_output_tokens: Some(100),
                    ..GenerateParams::default()
                }),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();
    kernel.inference(&mut session, &[]).await.unwrap();
}

// ---------------------------------------------------------------------------
// Budget is credit, and the kernel is the one that checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_with_no_credit_never_reaches_the_model() {
    let model = FakeModel::text(["never asked"]);
    let (kernel, store) = build(model);
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec::default(), // no grant: nothing may run
        )
        .await
        .unwrap();

    let refused = kernel.inference(&mut session, &[]).await;
    let error = refused.expect_err("a session with no credit cannot spend");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert!(error.reason().contains("token"), "{error:?}");

    // Closed, with the reason on the session, and the stop recorded once.
    assert!(session.is_closed());
    assert_eq!(session.stop_reason(), Some(nucleos::contract::StopReason::Tokens));
    let events = store.events_since(session.id(), 0).await.unwrap();
    let budgets = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::BudgetExhausted { .. }))
        .count();
    assert_eq!(budgets, 1);
}

#[tokio::test]
async fn a_second_call_after_exhaustion_does_not_write_a_second_stop() {
    let (kernel, store) = build(FakeModel::text(["unused"]));
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await
        .unwrap();

    // A driver that ignores the refusal and calls again must not be able to spend — and must not
    // turn one stop into two.
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(kernel.invoke_tools(&mut session, &[]).await.is_ok(), "an empty batch spends nothing");

    let events = store.events_since(session.id(), 0).await.unwrap();
    let budgets = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::BudgetExhausted { .. }))
        .count();
    assert_eq!(budgets, 1, "the stop is recorded once, not once per attempt");
}

#[tokio::test]
async fn a_tool_batch_is_gated_too() {
    use nucleos::adapter::{Tool, ToolCtx, ToolOutput};
    use nucleos::contract::ToolSpec;

    struct Any;
    #[async_trait::async_trait]
    impl Tool for Any {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "any".to_owned(),
                description: "Does nothing.".to_owned(),
                input_schema_json: "{}".to_owned(),
            }
        }
        async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
            ToolOutput::text("ran")
        }
    }

    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok"])))
        .tool(Arc::new(Any))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await
        .unwrap();

    let call = nucleos::contract::ToolCall {
        id: "call-1".to_owned(),
        name: "any".to_owned(),
        arguments_json: "{}".to_owned(),
        meta: std::collections::BTreeMap::new(),
    };
    let refused = kernel.invoke_tools(&mut session, &[call]).await;
    let error = refused.expect_err("a session with no credit cannot run a tool");
    assert_eq!(error.code(), ErrorCode::Budget);

    // And nothing ran: a gate that reports after the call is a bill, not a gate.
    let events = store.events_since(session.id(), 0).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolFinished { .. })),
        "the tool did not run"
    );
}
