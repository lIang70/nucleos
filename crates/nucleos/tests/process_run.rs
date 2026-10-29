//! Running a process, end to end.
//!
//! These are the only tests in the repository that spawn one, which is why they are also the only
//! ones that have to care about timing: the supervisor appends output from its own tasks.
//!
//! No sandbox mechanism is wired up yet, so everything here runs under `danger_full_access` — and
//! one test proves that anything stricter is *refused* rather than silently run unconfined.

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{AllowAll, FixedClock, MemoryStore, Store};
use nucleos::contract::{
    ErrorCode, EventKind, ExecStream, Principal, ProcessSpec, SandboxPolicy, SessionId, SessionKind,
};
use nucleos::kernel::{Kernel, SessionSpec};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Kernel, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();
    (kernel, store)
}

fn spec(argv: &[&str], sandbox: SandboxPolicy) -> SessionSpec {
    SessionSpec {
        kind: SessionKind::Process,
        process: Some(ProcessSpec {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            ..ProcessSpec::default()
        }),
        sandbox,
        ..SessionSpec::default()
    }
}

fn kinds(store: &MemoryStore, id: &SessionId) -> Vec<&'static str> {
    store
        .recorded(id)
        .iter()
        .map(|event| event.kind.type_name())
        .collect()
}

/// The supervisor writes from its own tasks, so output can land a moment after the exit.
fn output_text(store: &MemoryStore, id: &SessionId) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text: String = store
            .recorded(id)
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ExecOutput { chunk, .. } => Some(chunk.clone()),
                _ => None,
            })
            .collect();
        if !text.is_empty() || Instant::now() > deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
async fn a_command_runs_and_its_output_lands_in_the_log() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");

    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            spec(&["sh", "-c", "printf hi"], SandboxPolicy::DangerFullAccess),
        )
        .await?;
    assert_eq!(session.kind(), SessionKind::Process);

    let output = kernel.wait(&session).await?;
    assert_eq!(output.exit_code, 0);

    let seen = kinds(&store, &id);
    assert!(seen.contains(&"nucleos.v1.exec_started"), "{seen:?}");
    assert!(seen.contains(&"nucleos.v1.exec_exited"), "{seen:?}");
    assert_eq!(output_text(&store, &id), "hi");
    Ok(())
}

#[tokio::test]
async fn a_policy_without_a_mechanism_is_refused_rather_than_ignored() {
    let (kernel, _) = kernel();
    let error = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            spec(&["sh", "-c", "printf hi"], SandboxPolicy::read_only()),
        )
        .await
        .err()
        .unwrap();

    // Running unconfined while the policy claims otherwise is worse than refusing: the caller
    // would believe the isolation was real.
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(
        error.reason().contains("danger_full_access"),
        "{}",
        error.reason()
    );
}

#[tokio::test]
async fn stdin_reaches_the_process_and_is_recorded() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");

    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            spec(
                &["sh", "-c", "read line; printf '%s' \"$line\""],
                SandboxPolicy::DangerFullAccess,
            ),
        )
        .await?;

    kernel.write_stdin(&session, "hello\n").await?;
    let output = kernel.wait(&session).await?;

    assert_eq!(output.exit_code, 0);
    assert!(kinds(&store, &id).contains(&"nucleos.v1.exec_stdin"));
    assert_eq!(output_text(&store, &id), "hello");
    Ok(())
}

#[tokio::test]
async fn a_signal_ends_a_process_that_would_otherwise_run() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let session = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            spec(&["sleep", "30"], SandboxPolicy::DangerFullAccess),
        )
        .await?;

    kernel.signal(&session).await?;
    let output = kernel.wait(&session).await?;

    assert_ne!(
        output.exit_code, 0,
        "a killed process does not report success"
    );
    Ok(())
}

#[tokio::test]
async fn stderr_is_a_separate_stream() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");

    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            spec(
                &["sh", "-c", "printf out; printf err 1>&2"],
                SandboxPolicy::DangerFullAccess,
            ),
        )
        .await?;
    kernel.wait(&session).await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let streams: Vec<ExecStream> = store
            .recorded(&id)
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ExecOutput { stream, .. } => Some(*stream),
                _ => None,
            })
            .collect();
        if streams.contains(&ExecStream::Stdout) && streams.contains(&ExecStream::Stderr) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both streams should arrive: {streams:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
