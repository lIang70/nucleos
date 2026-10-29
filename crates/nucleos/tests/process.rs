//! Process sessions: the same lifecycle as an agent, a different face.
//!
//! Three things are pinned here. A process session cannot exist without something to run; the
//! two faces do not mix; and a process that outlived the runtime is reported as lost rather than
//! as still running.

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{AllowAll, FakeModel, FixedClock, MemoryStore, Store};
use nucleos::contract::{
    ErrorCode, Principal, ProcessSpec, ProcessState, ProcessStatus, Routing, SandboxPermissions,
    SandboxPolicy, SessionId, SessionKind,
};
use nucleos::kernel::{Kernel, SessionSpec};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Kernel, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
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

fn process_spec() -> ProcessSpec {
    ProcessSpec {
        argv: vec!["python3".to_owned(), "-c".to_owned(), "print(1)".to_owned()],
        cwd: Some("/work".to_owned()),
        env: Default::default(),
        permissions: SandboxPermissions::UseDefault,
    }
}

#[tokio::test]
async fn a_process_session_without_a_process_is_invalid() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let error = kernel
        .open(
            &principal(),
            SessionId::new("p1"),
            SessionSpec {
                kind: SessionKind::Process,
                process: None,
                ..SessionSpec::default()
            },
        )
        .await
        .err()
        .unwrap();

    // A process session with nothing to run is a request that cannot mean anything.
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn a_process_session_records_how_it_was_started() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");
    kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                kind: SessionKind::Process,
                process: Some(process_spec()),
                workspace: Some("/work".to_owned()),
                sandbox: SandboxPolicy::read_only(),
                ..SessionSpec::default()
            },
        )
        .await?;

    let snapshot = store.snapshot(&id).await?.unwrap();
    assert_eq!(snapshot.kind, SessionKind::Process);
    let process = snapshot.process.unwrap();
    assert_eq!(process.spec.argv[0], "python3");
    // Pending, not Running: the session exists and the process does not — and saying "running"
    // before a pid exists is the lie that makes a lost process indistinguishable from a live one.
    assert_eq!(process.status, ProcessStatus::Pending);
    assert!(!process.is_finished());

    let reloaded = kernel.load(&principal(), &id).await?;
    assert_eq!(reloaded.kind(), SessionKind::Process);
    assert_eq!(
        reloaded.process().map(|process| &process.spec.argv[0]),
        Some(&"python3".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn an_agent_session_has_no_process() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("a1");
    kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                // A spec handed to an agent session is ignored, not silently used.
                process: Some(process_spec()),
                ..SessionSpec::default()
            },
        )
        .await?;

    let snapshot = store.snapshot(&id).await?.unwrap();
    assert_eq!(snapshot.kind, SessionKind::Agent);
    assert!(snapshot.process.is_none());
    Ok(())
}

#[tokio::test]
async fn the_faces_do_not_mix() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("p1"),
            SessionSpec {
                kind: SessionKind::Process,
                process: Some(process_spec()),
                ..SessionSpec::default()
            },
        )
        .await?;

    let error = kernel.inference(&mut session, &[]).await.err().unwrap();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("no model calls"));
    Ok(())
}

#[test]
fn a_process_that_outlived_the_runtime_is_lost_not_running() {
    // A live process cannot be reconstructed from a log: its memory, its descriptors and its
    // cursor are not in the event stream. So the honest status is `Lost`, and it counts as
    // finished — the alternative is a session that waits forever for a process that is gone.
    let state = ProcessState {
        spec: process_spec(),
        status: ProcessStatus::Lost,
    };
    assert!(state.is_finished());

    let running = ProcessState {
        spec: process_spec(),
        status: ProcessStatus::Running { pid: 42 },
    };
    assert!(!running.is_finished());
}
