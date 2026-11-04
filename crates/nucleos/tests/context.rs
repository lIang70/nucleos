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
        credit_tokens: Some(1_000_000),
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
            SessionSpec {
                credit_tokens: Some(0),
                ..SessionSpec::default()
            }, // a zero grant: nothing may run
        )
        .await
        .unwrap();

    let refused = kernel.inference(&mut session, &[]).await;
    let error = refused.expect_err("a session with no credit cannot spend");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert!(error.reason().contains("token"), "{error:?}");

    // Closed, with the reason on the session, and the stop recorded once.
    assert!(session.is_closed());
    assert_eq!(
        session.stop_reason(),
        Some(nucleos::contract::StopReason::Tokens)
    );
    let events = store.events_since(&key(session.id()), 0).await.unwrap();
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
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(0),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // A driver that ignores the refusal and calls again must not be able to spend — and must not
    // turn one stop into two.
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(
        kernel.invoke_tools(&mut session, &[]).await.is_ok(),
        "an empty batch spends nothing"
    );

    let events = store.events_since(&key(session.id()), 0).await.unwrap();
    let budgets = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::BudgetExhausted { .. }))
        .count();
    assert_eq!(
        budgets, 1,
        "the stop is recorded once, not once per attempt"
    );
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
                isolation: Some(nucleos::contract::Isolation::InProcess),
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
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(0),
                ..SessionSpec::default()
            },
        )
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
    let events = store.events_since(&key(session.id()), 0).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolFinished { .. })),
        "the tool did not run"
    );
}

/// A session that ran out of credit can be topped up, and one that stopped on a budget can be revived.
#[tokio::test]
async fn credit_revives_a_session_that_stopped_on_a_budget() {
    let (kernel, store) = build(FakeModel::text(["ok"]));
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(0),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // Exhausted on the first call, as the zero grant says.
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(session.is_closed());

    // Revived by a grant, and open again.
    kernel.credit(&mut session, Some(10_000)).await.unwrap();
    assert!(!session.is_closed());
    assert_eq!(session.stop_reason(), None);
    assert_eq!(session.credit(), Some(10_000));

    // The grant is in the log: a replay cannot mistake this for a session that never stopped.
    let events = store.events_since(&key(session.id()), 0).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Credited { .. })),
        "no Credited event: {events:?}"
    );

    // And the revival survives a reload, which is the whole point of recording it.
    let reloaded = kernel.load(&principal(), session.id()).await.unwrap();
    assert!(!reloaded.is_closed());
    assert_eq!(reloaded.credit(), Some(10_000));
}

/// A session that ended for any other reason is not revived by money.
#[tokio::test]
async fn credit_does_not_revive_a_session_that_was_cancelled() {
    let (kernel, _store) = build(FakeModel::text(["ok"]));
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await
        .unwrap();
    kernel
        .close(&mut session, nucleos::contract::StopReason::Cancelled)
        .await
        .unwrap();

    let error = kernel.credit(&mut session, Some(1_000)).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(session.is_closed(), "a cancelled session stays closed");
}

/// A grant of nothing is not a grant, and removing the limit is explicit.
#[tokio::test]
async fn credit_of_zero_is_refused_and_none_removes_the_limit() {
    let (kernel, _store) = build(FakeModel::text(["ok"]));
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(100),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    assert!(kernel.credit(&mut session, Some(0)).await.is_err());

    kernel.credit(&mut session, None).await.unwrap();
    assert_eq!(session.credit(), None, "no limit after the grant");
}

/// A grant cannot revive a session whose ceiling is what stopped it.
///
/// `max_steps`, `max_tokens`, `max_cost` and `max_wall` are fixed at `Start`. Reviving one of those
/// would report a session open with no stop reason and then refuse its very next call — a lie about
/// what the caller can do next, in exchange for nothing.
#[tokio::test]
async fn credit_does_not_revive_a_session_stopped_by_a_ceiling() {
    let (kernel, _store) = build(FakeModel::text(["ok"]));
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(1_000),
                budget: nucleos::contract::Budget {
                    // A step cap: immutable after `Start`, and reached without the model having to
                    // report tokens.
                    max_steps: Some(1),
                    ..nucleos::contract::Budget::default()
                },
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // The first turn uses the only step the session has; the second is refused on that cap, which is
    // what stops it — a ceiling, not a shortage of credit.
    kernel.inference(&mut session, &[]).await.unwrap();
    assert!(kernel.inference(&mut session, &[]).await.is_err());
    assert!(session.is_closed());
    let stopped = session.stop_reason();

    // A grant is refused, and changes nothing — not the balance, not the state, not the reason.
    let refused = kernel.credit(&mut session, Some(1_000_000)).await;
    let error = refused.expect_err("a ceiling stop is not revivable by credit");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert!(error.reason().contains("ceiling"), "{error:?}");
    assert!(session.is_closed(), "a refused grant revived the session");
    assert_eq!(
        session.stop_reason(),
        stopped,
        "a refused grant changed the reason"
    );
    assert_eq!(
        session.credit(),
        Some(1_000),
        "a refused grant changed the balance"
    );
}
