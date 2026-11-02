//! Budgets across the boundary every request crosses.
//!
//! Over the ABI a session is opened once and loaded on every call, so a limit that lives only in the
//! handle a request happened to get is a limit that applies to the first request and then forgets
//! itself. These tests take the reload seriously: the budget has to survive it, and the wall clock has
//! to mean "how long this session has run" rather than "how long ago it was opened".

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Budget, ErrorCode, Principal, Routing, SessionId, StopReason};
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
                credit_tokens: 1_000_000,
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
                credit_tokens: 1_000_000,
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
    kernel.checkpoint(&session).await.unwrap();

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
    kernel.checkpoint(&session).await.unwrap();
    session = kernel.load(&principal(), &id).await.unwrap();
    let error = kernel
        .inference(&mut session, &[])
        .await
        .expect_err("six seconds of running is past a five second budget");
    assert_eq!(error.code(), ErrorCode::Budget);
    assert_eq!(session.stop_reason(), Some(StopReason::Wall));
}
