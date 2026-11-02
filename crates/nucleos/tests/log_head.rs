//! One log, one counter, one lock — however many handles point at it.
//!
//! `Session` is a handle, and `load` mints a new one per request. The counter and the lock used to be
//! minted with it, so two requests on one session held two counters: each reserved the same number and
//! the store refused the second as a gap, losing that event. These tests are about the log's identity,
//! not about its contents.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Principal, Routing, SessionId};
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

fn kernel() -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok"])))
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
        credit_tokens: 1_000_000,
        ..SessionSpec::default()
    }
}

#[tokio::test]
async fn two_handles_of_one_session_keep_the_log_dense() {
    let (kernel, store) = kernel();
    let id = SessionId::new("s1");
    kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Two handles, as two concurrent requests would have.
    let mut first = kernel.load(&principal(), &id).await.unwrap();
    let second = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(
        first.next_seq(),
        second.next_seq(),
        "handles of one session share the log's counter"
    );

    // Both write, at once. With a counter per handle they reserve the same number and the store refuses
    // the loser as a gap; with one head, they serialise and both land.
    let (a, b) = tokio::join!(kernel.configure(&mut first, Default::default()), async {
        let mut second = second;
        kernel.configure(&mut second, Default::default()).await
    });
    a.expect("the first write lands");
    b.expect("the second write lands");

    let events = store
        .events_since(&nucleos::adapter::SessionKey::new("acme", id), 0)
        .await
        .unwrap();
    let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(
        seqs,
        (0..u64::try_from(seqs.len()).unwrap()).collect::<Vec<_>>(),
        "the log is dense: {seqs:?}"
    );
}

#[tokio::test]
async fn a_handle_loaded_while_a_process_writes_joins_the_same_head() {
    // The supervisor keeps the `Arc` it was spawned with. A handle loaded afterwards must resolve to the
    // same one, or `close` would hand the log a number the reader has already used.
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");
    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            SessionSpec {
                kind: nucleos::contract::SessionKind::Process,
                process: Some(nucleos::contract::ProcessSpec {
                    argv: vec![
                        "sh".to_owned(),
                        "-c".to_owned(),
                        // Long enough that the readers are still running when the second handle closes.
                        "printf a; sleep 0.3; printf b".to_owned(),
                    ],
                    ..Default::default()
                }),
                sandbox: nucleos::contract::SandboxPolicy::DangerFullAccess,
                ..spec()
            },
        )
        .await
        .unwrap();

    let mut other = kernel.load(&principal(), &id).await.unwrap();
    kernel
        .close(&mut other, nucleos::contract::StopReason::Completed)
        .await
        .expect("closing from another handle must not fight the readers for a number");

    kernel.wait(&session).await.unwrap();
    let events = store
        .events_since(&nucleos::adapter::SessionKey::new("acme", id), 0)
        .await
        .unwrap();
    let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(
        seqs,
        (0..u64::try_from(seqs.len()).unwrap()).collect::<Vec<_>>(),
        "the log is dense: {seqs:?}"
    );
    // And the output survived the interleaving rather than being dropped by a refused append.
    let text: String = events
        .iter()
        .filter_map(|event| match &event.kind {
            nucleos::contract::EventKind::ExecOutput { chunk, .. } => Some(chunk.clone()),
            _ => None,
        })
        .collect();
    assert!(text.contains('a') && text.contains('b'), "{text:?}");
}
