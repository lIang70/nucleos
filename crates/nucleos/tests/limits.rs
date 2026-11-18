//! Budgets across the boundary every request crosses.
//!
//! Over the ABI a session is opened once and loaded on every call, so a limit that lives only in the
//! handle a request happened to get is a limit that applies to the first request and then forgets
//! itself. These tests take the reload seriously: the budget has to survive it, and the wall clock has
//! to mean "how long this session has run" rather than "how long ago it was opened".

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Budget, ErrorCode, Principal, Routing, SessionId, StopReason, Usage};
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

fn kernel(clock: &Arc<FixedClock>) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok", "ok", "ok"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::clone(clock) as Arc<dyn nucleos::adapter::Clock>)
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (kernel, store)
}

#[tokio::test]
async fn a_step_budget_survives_the_reload_every_request_does() {
    let clock = Arc::new(FixedClock::at(0));
    let (kernel, _) = kernel(&clock);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
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

    session = {
        kernel.inference(&mut session, &[]).await.unwrap();
        kernel.load(&principal(), &id).await.unwrap()
    };

    // The second call is a new request and therefore a new handle. Before the budget was in the
    // checkpoint this succeeded, and every `max_*` a caller set applied to the first turn only.
    let error = kernel
        .inference(&mut session, &[])
        .await
        .expect_err("the step budget was reached on the first turn");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert_eq!(session.stop_reason(), Some(StopReason::Steps));
}

#[tokio::test]
async fn wall_millis_are_charged_to_running_and_not_to_waiting() {
    let clock = Arc::new(FixedClock::at(0));
    let (kernel, _) = kernel(&clock);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(1_000_000),
                budget: Budget {
                    max_wall_millis: Some(5_000),
                    ..Budget::default()
                },
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // Four seconds of running, written into the checkpoint the way a request's ending would.
    clock.advance(4_000);
    kernel.checkpoint(&mut session).await.unwrap();

    // A minute passes between requests. That is nobody's model time, and a session parked on a human
    // verdict must not exhaust its budget while it waits.
    clock.advance(60_000);
    session = kernel.load(&principal(), &id).await.unwrap();
    kernel
        .inference(&mut session, &[])
        .await
        .expect("an idle gap is not run time");

    // Two more seconds of running brings the total past the limit.
    clock.advance(2_000);
    kernel.checkpoint(&mut session).await.unwrap();
    session = kernel.load(&principal(), &id).await.unwrap();
    let error = kernel
        .inference(&mut session, &[])
        .await
        .expect_err("six seconds of running is past a five second budget");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert_eq!(session.stop_reason(), Some(StopReason::Wall));
}

/// Answers, and says exactly what it cost — the one shape that can trip a cost ceiling.
struct Bills(Usage);

#[async_trait]
impl nucleos::adapter::Model for Bills {
    fn capability(&self) -> nucleos::contract::Capability {
        nucleos::contract::Capability {
            inputs: Vec::new(),
            outputs: Vec::new(),
            tool_calling: true,
            tool_choice_required: true,
            structured_output: true,
            context_window: 128_000,
            max_output_tokens: 4_096,
            max_input_bytes: None,
        }
    }

    fn count_tokens(
        &self,
        request: &nucleos::adapter::ModelRequest,
    ) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(
        &self,
        _prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<nucleos::adapter::ModelStream> {
        let deltas = vec![
            Ok(nucleos::adapter::ModelDelta::PartAppend {
                part_index: 0,
                part: nucleos::contract::Part::text("ok"),
            }),
            Ok(nucleos::adapter::ModelDelta::Usage(self.0.clone())),
            Ok(nucleos::adapter::ModelDelta::Finished {
                reason: nucleos::adapter::FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(deltas)))
    }
}

fn billing(usage: Usage) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(Bills(usage)))
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

async fn open_with(kernel: &Kernel, budget: Budget) -> (SessionId, nucleos::kernel::Session) {
    let id = SessionId::new("s1");
    let session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: Some(1_000_000),
                budget,
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    (id, session)
}

#[tokio::test]
async fn a_cost_ceiling_stops_the_session() {
    // Two of the four budget dimensions had no test at all, and `StopReason::Cost` was never produced
    // anywhere in the workspace: a money control nothing had ever watched fire. The check is
    // `spent >= max`, so one turn that costs more than the ceiling refuses the next one.
    let (kernel, _) = billing(Usage {
        input_tokens: 7,
        output_tokens: 3,
        micros_usd: Some(120),
    });
    let (_id, mut session) = open_with(
        &kernel,
        Budget {
            max_micros_usd: Some(100),
            ..Budget::default()
        },
    )
    .await;

    kernel.inference(&mut session, &[]).await.unwrap();
    let refused = kernel.inference(&mut session, &[]).await;
    let Err(error) = refused else {
        panic!("a session that spent more than its ceiling ran another turn");
    };
    assert_eq!(error.code(), ErrorCode::Budget);
    assert_eq!(session.stop_reason(), Some(StopReason::Cost));
}

#[tokio::test]
async fn a_token_ceiling_stops_the_session() {
    // Distinct from `credit == 0`: this is the ceiling fixed at `Start`, which a grant cannot move.
    let (kernel, _) = billing(Usage {
        input_tokens: 10,
        output_tokens: 0,
        micros_usd: None,
    });
    let (_id, mut session) = open_with(
        &kernel,
        Budget {
            max_tokens: Some(10),
            ..Budget::default()
        },
    )
    .await;

    kernel.inference(&mut session, &[]).await.unwrap();
    let Err(error) = kernel.inference(&mut session, &[]).await else {
        panic!("a session that spent every token it was allowed ran another turn");
    };
    assert_eq!(error.code(), ErrorCode::Budget);
    assert_eq!(session.stop_reason(), Some(StopReason::Tokens));
}

#[tokio::test]
async fn an_unreported_cost_never_trips_the_cost_ceiling() {
    // The trade-off the ledger makes everywhere else: an unreported counter stays unreported rather
    // than being counted as zero. A cost ceiling therefore cannot fire on a model that does not say
    // what it cost — which is worth pinning, because `spent >= max` would otherwise be satisfied by a
    // zero nobody reported.
    let (kernel, _) = billing(Usage {
        input_tokens: 1,
        output_tokens: 1,
        micros_usd: None,
    });
    let (_id, mut session) = open_with(
        &kernel,
        Budget {
            max_micros_usd: Some(0),
            ..Budget::default()
        },
    )
    .await;

    kernel.inference(&mut session, &[]).await.unwrap();
    kernel.inference(&mut session, &[]).await.unwrap();
    assert!(
        session.stop_reason().is_none(),
        "an unreported cost stopped the session"
    );
}
