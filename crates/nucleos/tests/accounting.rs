//! The books: what a session spent, survives the boundary every request crosses.
//!
//! A bill that is rebuilt from zero on every load under-reports exactly the sessions that were
//! interrupted, which are the ones somebody is reconciling. And a cancelled attempt that is recorded as
//! having cost nothing is worse than no record: it says "this was free", which no provider believes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{Budget, Capability, ErrorCode, Principal, Routing, SessionId, Usage};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;

const SPENT: Usage = Usage {
    input_tokens: 7,
    output_tokens: 3,
    micros_usd: Some(120),
};

/// Answers, and says what it cost.
struct Billing;

#[async_trait]
impl Model for Billing {
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
        let deltas = vec![
            Ok(ModelDelta::Text {
                part_index: 0,
                text: "hi".to_owned(),
            }),
            Ok(ModelDelta::Usage(SPENT)),
            Ok(ModelDelta::Finished {
                reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(deltas)))
    }
}

/// Bills first, then stalls — the shape of a turn somebody cancels to stop paying.
struct Stalling;

#[async_trait]
impl Model for Stalling {
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
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            let _ = sender.send(Ok(ModelDelta::Usage(SPENT))).await;
            // And never finishes on its own.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel(model: Arc<dyn Model>) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", model)
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
        credit_tokens: Some(1_000_000),
        ..SessionSpec::default()
    }
}

#[tokio::test]
async fn the_bill_survives_the_reload_every_request_does() {
    let (kernel, _) = kernel(Arc::new(Billing));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    kernel.inference(&mut session, &[]).await.unwrap();

    let spent = session.ledger().total.clone();
    assert_eq!(spent.input_tokens, SPENT.input_tokens);

    // The next request is a new handle. It has to know what the first one cost.
    let reloaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(
        reloaded.ledger().total,
        spent,
        "the ledger is the record, not a statistic"
    );
    assert_eq!(
        reloaded.usage(),
        session.usage(),
        "and the spend agrees with it"
    );
}

#[tokio::test]
async fn a_cancelled_turn_is_billed_for_what_it_streamed() {
    let (kernel, _) = kernel(Arc::new(Stalling));
    let kernel = Arc::new(kernel);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    let id = session.id().clone();

    let canceller = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move {
            // The model bills before it stalls, so a cancel that lands after the usage delta is the
            // case this test is about.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            kernel.cancel(&nucleos::adapter::SessionKey::new("acme", id))
        })
    };

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert!(canceller.await.unwrap());

    let attempt = session
        .ledger()
        .attempts
        .last()
        .expect("the cancelled attempt is recorded");
    assert_eq!(
        attempt.usage.input_tokens, SPENT.input_tokens,
        "a ledger that says zero claims the provider billed nothing"
    );
    assert_eq!(session.ledger().total.input_tokens, SPENT.input_tokens);
    assert_eq!(session.usage().input_tokens, SPENT.input_tokens);
}

#[tokio::test]
async fn a_failed_step_counts_against_the_step_budget() {
    let (kernel, _) = kernel(Arc::new(Stalling));
    let kernel = Arc::new(kernel);
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(1_000_000),
                budget: Budget {
                    max_steps: Some(1),
                    ..Budget::default()
                },
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    let id = session.id().clone();

    let canceller = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            kernel.cancel(&nucleos::adapter::SessionKey::new("acme", id))
        })
    };
    let _ = kernel.inference(&mut session, &[]).await;
    assert!(canceller.await.unwrap());

    // The cancelled turn was still a turn. Without this, a retry loop could spend steps forever.
    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Budget);
}
