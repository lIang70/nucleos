//! Running a process, end to end.
//!
//! These are the only tests in the repository that spawn one, which is why they are also the only
//! ones that have to care about timing: the supervisor appends output from its own tasks.
//!
//! Most of these run under `danger_full_access`, because they are about the process and not about the
//! policy. One test proves that anything stricter is *enforced or refused*, never silently run
//! unconfined — and it decides which of the two at run time, because that depends on what the host
//! can do, not on how it was compiled.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{
    EventKind, ExecStream, Principal, ProcessSpec, Routing, SandboxPolicy, SessionId, SessionKind,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn kernel() -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
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

async fn kinds(store: &SqliteStore, id: &SessionId) -> Vec<&'static str> {
    store
        .events_since(&key(id), 0)
        .await
        .unwrap()
        .iter()
        .map(|event| event.kind.type_name())
        .collect()
}

/// The supervisor writes from its own tasks, so output can land a moment after the exit.
async fn output_text(store: &SqliteStore, id: &SessionId) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text: String = store
            .events_since(&key(id), 0)
            .await
            .unwrap()
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
async fn wait_answers_with_the_output_not_just_an_exit_code() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let session = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            spec(
                &["sh", "-c", "printf out; printf err 1>&2"],
                SandboxPolicy::DangerFullAccess,
            ),
        )
        .await?;

    let output = kernel.wait(&session).await?;

    // An exit code with no output is not a result. The event stream carries the same text
    // incrementally, for a client watching it happen; `wait` carries it once, for a client that
    // wants the answer.
    assert_eq!(output.stdout.text, "out");
    assert_eq!(output.stderr.text, "err");
    assert_eq!(output.exit_code, 0);
    Ok(())
}

#[tokio::test]
async fn the_exit_event_carries_the_summary_and_leaves_the_text_in_the_stream()
-> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");
    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            spec(&["sh", "-c", "printf hi"], SandboxPolicy::DangerFullAccess),
        )
        .await?;
    kernel.wait(&session).await?;

    let exit = store
        .events_since(&key(&id), 0)
        .await
        .unwrap()
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ExecExited { output } => Some(output),
            _ => None,
        })
        .unwrap();

    // A log that repeats the output would be a log with two truths; the chunks are already there.
    assert_eq!(exit.stdout.text, "");
    assert_eq!(exit.exit_code, 0);
    Ok(())
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

    let seen = kinds(&store, &id).await;
    assert!(seen.contains(&"nucleos.v1.exec_started"), "{seen:?}");
    assert!(seen.contains(&"nucleos.v1.exec_exited"), "{seen:?}");
    assert_eq!(output_text(&store, &id).await, "hi");
    Ok(())
}

/// A confining policy is either enforced or refused — never ignored.
///
/// Which of the two depends on the platform, and both are correct answers: what is not allowed is
/// running the command unconfined while the policy claims otherwise, because then the caller would
/// believe the isolation was real.
#[tokio::test]
async fn a_confining_policy_is_enforced_or_refused_never_ignored() {
    let (kernel, _) = kernel();
    let spawned = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            spec(&["sh", "-c", "printf hi"], SandboxPolicy::read_only()),
        )
        .await;

    // Enforced, or refused — decided by what this host can actually do. The compile-time split this
    // test used to have ("macOS enforces, everything else refuses") was wrong on the first non-macOS
    // machine that ran it: Linux has bubblewrap, CI installs it, and the spawn succeeded.
    match spawned {
        Ok(session) => {
            if !mechanism_can_run() {
                eprintln!(
                    "skipping the enforcement half: this host has the mechanism but cannot start it"
                );
                return;
            }
            // Enforced — and the record says by what. A policy that was silently ignored would
            // produce a running session with no mechanism, which is the failure this test names.
            let started = kernel
                .events(&principal(), session.id(), 0)
                .await
                .unwrap()
                .into_iter()
                .find_map(|event| match event.kind {
                    EventKind::ExecStarted { mechanism, .. } => Some(mechanism),
                    _ => None,
                })
                .expect("the start is recorded");
            assert!(
                started.is_some(),
                "a confining policy ran a command with no mechanism: it was ignored"
            );
            let output = kernel.wait(&session).await.unwrap();
            assert_eq!(
                output.exit_code, 0,
                "a read-only policy still runs a command"
            );
        }
        Err(error) => {
            // Refused — and for the reason the contract promises, so an operator knows what to do.
            assert_eq!(error.code(), nucleos::contract::ErrorCode::Unavailable);
            assert!(
                error.reason().contains("danger_full_access")
                    || error.reason().contains("bubblewrap")
                    || error.reason().contains("seatbelt"),
                "{}",
                error.reason()
            );
        }
    }
}

/// Whether this host can actually start the mechanism it has.
///
/// `confine` checks that the program is on the path; whether the kernel lets it create namespaces is a
/// different question, and one that only a probe answers. The same probe `tests/sandbox.rs` uses.
#[cfg(target_os = "linux")]
fn mechanism_can_run() -> bool {
    std::process::Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn mechanism_can_run() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mechanism_can_run() -> bool {
    true
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
    assert!(kinds(&store, &id).await.contains(&"nucleos.v1.exec_stdin"));
    assert_eq!(output_text(&store, &id).await, "hello");
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
            .events_since(&key(&id), 0)
            .await
            .unwrap()
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

#[tokio::test]
async fn the_listing_is_durable_and_only_counts_process_sessions() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("p1");
    let session = kernel
        .spawn(
            &principal(),
            id.clone(),
            spec(&["sh", "-c", "printf hi"], SandboxPolicy::DangerFullAccess),
        )
        .await?;
    kernel.wait(&session).await?;

    // An agent session exists in the same store and must not appear: the question is "which exec
    // sessions were created", not "which sessions exist".
    kernel
        .open(&principal(), SessionId::new("a1"), SessionSpec::default())
        .await?;

    let listed = kernel
        .processes("acme", &nucleos::contract::ProcessQuery::default())
        .await?;
    assert_eq!(listed.processes.len(), 1);
    assert_eq!(listed.processes[0].0, id);
    assert!(matches!(
        listed.processes[0].1.status,
        nucleos::contract::ProcessStatus::Exited { .. }
    ));

    // The listing is durable: a kernel that has never seen the process still knows it ran.
    let fresh = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()?;
    let listed = fresh
        .processes("acme", &nucleos::contract::ProcessQuery::default())
        .await?;
    assert_eq!(listed.processes.len(), 1);
    assert!(matches!(
        listed.processes[0].1.status,
        nucleos::contract::ProcessStatus::Exited { .. }
    ));

    // And a checkpoint that says "running" with no supervisor entry is a session that outlived the
    // runtime: the honest answer is `Lost`, not "running".
    let mut snapshot = store.snapshot(&key(&id)).await?.unwrap();
    snapshot.process.as_mut().unwrap().status =
        nucleos::contract::ProcessStatus::Running { pid: 1 };
    // A new version, because the store refuses a write that is not newer than what it holds — which is
    // the invariant this test now sits on top of.
    snapshot.version = snapshot.version.saturating_add(1);
    store.write_snapshot(snapshot).await?;
    let listed = fresh
        .processes("acme", &nucleos::contract::ProcessQuery::default())
        .await?;
    assert_eq!(
        listed.processes[0].1.status,
        nucleos::contract::ProcessStatus::Lost
    );
    Ok(())
}

#[tokio::test]
async fn a_page_fills_through_a_phase_filter_and_says_where_to_continue() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    // Three that end, and two that do not, so a filter has something to skip between them.
    for index in 0..3 {
        let session = kernel
            .spawn(
                &principal(),
                SessionId::new(format!("done-{index}")),
                spec(&["sh", "-c", "true"], SandboxPolicy::DangerFullAccess),
            )
            .await?;
        kernel.wait(&session).await?;
    }
    let mut running = Vec::new();
    for index in 0..2 {
        running.push(
            kernel
                .spawn(
                    &principal(),
                    SessionId::new(format!("live-{index}")),
                    spec(&["sleep", "30"], SandboxPolicy::DangerFullAccess),
                )
                .await?,
        );
    }

    // One page of two, filtered to the ones that ended: the page has to be *filled*, not just
    // truncated, so the kernel walks the store until it has two rows or runs out.
    let page = kernel
        .processes(
            "acme",
            &nucleos::contract::ProcessQuery {
                phase: Some(nucleos::contract::ProcessPhase::Exited),
                limit: 2,
                after: None,
            },
        )
        .await?;
    assert_eq!(page.processes.len(), 2);
    assert!(page.next.is_some(), "there is a third");

    // The cursor resumes after the last row examined, so nothing is visited twice and nothing is
    // skipped.
    let next = kernel
        .processes(
            "acme",
            &nucleos::contract::ProcessQuery {
                phase: Some(nucleos::contract::ProcessPhase::Exited),
                limit: 2,
                after: page.next.clone(),
            },
        )
        .await?;
    assert_eq!(next.processes.len(), 1);
    assert_eq!(next.next, None, "the third was the last");

    // A cursor that lands between rows of different phases: the page must find the finished ones
    // after it and skip the running ones, without losing the last row it examined.
    let tail = kernel
        .spawn(
            &principal(),
            SessionId::new("zzz-done"),
            spec(&["sh", "-c", "true"], SandboxPolicy::DangerFullAccess),
        )
        .await?;
    kernel.wait(&tail).await?;
    let after_running = kernel
        .processes(
            "acme",
            &nucleos::contract::ProcessQuery {
                phase: Some(nucleos::contract::ProcessPhase::Exited),
                limit: 2,
                after: Some(nucleos::contract::SessionId::new("live-0")),
            },
        )
        .await?;
    assert_eq!(
        after_running
            .processes
            .iter()
            .map(|(session, _)| session.as_str())
            .collect::<Vec<_>>(),
        ["zzz-done"],
        "the running row between them must be skipped, not duplicated or lost"
    );
    assert_eq!(after_running.next, None, "nothing follows the last row");

    // A filter that matches nothing is an empty page, not an error.
    let none = kernel
        .processes(
            "acme",
            &nucleos::contract::ProcessQuery {
                phase: Some(nucleos::contract::ProcessPhase::Pending),
                limit: 10,
                after: None,
            },
        )
        .await?;
    assert!(none.processes.is_empty());

    for session in running {
        let _ = kernel.signal(&session).await;
        let _ = kernel.wait(&session).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// A process session outlives the runtime that started it
// ---------------------------------------------------------------------------

/// A kernel on a file, so a second one can be built on the same file after this one is dropped.
fn kernel_on(path: &std::path::Path) -> nucleos::Result<Kernel> {
    Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::new(SqliteStore::open(path)?))
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
}

#[tokio::test]
async fn a_resumed_process_session_can_still_answer_what_it_did() -> nucleos::Result<()> {
    let directory = std::env::temp_dir().join(format!("nucleos-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let database = directory.join("nucleos.db");

    let id = SessionId::new("p1");
    let (before, during) = {
        let kernel = kernel_on(&database)?;
        let session = kernel
            .spawn(
                &principal(),
                id.clone(),
                SessionSpec {
                    sandbox: SandboxPolicy::DangerFullAccess,
                    credit_tokens: 1_000,
                    process: Some(ProcessSpec {
                        argv: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "printf out; printf err >&2".to_owned(),
                        ],
                        cwd: None,
                        env: Default::default(),
                        permissions: Default::default(),
                    }),
                    ..SessionSpec::default()
                },
            )
            .await?;
        let before = kernel.wait(&session).await?;
        (before, session)
    };

    // Nothing of the first kernel is left — this is the restart, not a reload.
    assert_eq!(before.stdout.text, "out");
    assert_eq!(before.stderr.text, "err");
    drop(during);

    let kernel = kernel_on(&database)?;
    let resumed = kernel.load(&principal(), &id).await?;
    let after = kernel.wait(&resumed).await?;

    // The supervisor is memory and has forgotten this process. The answer is in the log: the chunks
    // the reader wrote, and the summary the exit wrote.
    assert_eq!(after.exit_code, 0);
    assert_eq!(
        after.stdout.text, "out",
        "the text comes back from the stream"
    );
    assert_eq!(after.stderr.text, "err");
    assert_eq!(
        after.duration_millis, before.duration_millis,
        "the summary is the record"
    );

    let _ = std::fs::remove_dir_all(&directory);
    Ok(())
}

#[tokio::test]
async fn a_process_with_no_recorded_exit_says_so_instead_of_inventing_one() -> nucleos::Result<()> {
    let (kernel, _store) = kernel();
    // Opened, never spawned: pending, no chunks, no exit. This is the shape of a session whose
    // process this runtime never started, and the one thing it must not do is answer with a zero
    // exit code and empty output.
    let session = kernel
        .open(
            &principal(),
            SessionId::new("p1"),
            SessionSpec {
                kind: SessionKind::Process,
                sandbox: SandboxPolicy::DangerFullAccess,
                credit_tokens: 1_000,
                process: Some(ProcessSpec {
                    argv: vec!["/usr/bin/sleep".to_owned(), "10".to_owned()],
                    cwd: None,
                    env: Default::default(),
                    permissions: Default::default(),
                }),
                ..SessionSpec::default()
            },
        )
        .await?;

    let error = kernel
        .wait(&session)
        .await
        .expect_err("a process with no exit has no answer");
    assert!(error.reason().contains("no recorded exit"), "{error:?}");
    Ok(())
}

/// A child that never reads its stdin must still be killable.
///
/// `write_stdin` and `signal` both hold the supervisor's map. When the child stops reading, the pipe
/// fills, the supervisor blocks writing to it, and the control channel behind it fills too — so a
/// writer waits. If it waits *holding the map lock*, `signal` queues behind it and nothing on the node
/// can be killed at exactly the moment killing is what you want.
#[tokio::test]
async fn a_child_that_does_not_read_stdin_can_still_be_killed() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let kernel = Arc::new(kernel);
    let session = Arc::new(
        kernel
            .spawn(
                &principal(),
                SessionId::new("stuck"),
                spec(&["sh", "-c", "sleep 30"], SandboxPolicy::DangerFullAccess),
            )
            .await?,
    );

    // Far more than a pipe (64 KiB) and a channel (64 entries): the writer is meant to block.
    let writer = {
        let kernel = Arc::clone(&kernel);
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            let chunk = "x".repeat(1024);
            for _ in 0..400 {
                if kernel.write_stdin(&session, &chunk).await.is_err() {
                    return;
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), kernel.signal(&session))
        .await
        .expect("the kill path must not queue behind a blocked writer")
        .expect("the signal reaches the supervisor");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "signal took {:?}",
        started.elapsed()
    );

    kernel.wait(&session).await?;
    writer.abort();
    Ok(())
}

/// A `wait` in flight must not hide the process from everything else.
///
/// The entry used to be removed *before* awaiting the exit, so a client that disconnected mid-wait left
/// a live child that `signal`, `stdin` and `state` all reported as gone.
#[tokio::test]
async fn a_wait_in_flight_does_not_hide_the_process() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let kernel = Arc::new(kernel);
    let session = kernel
        .spawn(
            &principal(),
            SessionId::new("waited"),
            spec(&["sh", "-c", "sleep 1"], SandboxPolicy::DangerFullAccess),
        )
        .await?;
    let id = session.id().clone();

    // The session is moved into the waiter, which is what a request holding it looks like; the test
    // keeps only the id, because that is all an operator has.
    let waiter = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move { kernel.wait(&session).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Still running, still addressable.
    let page = kernel
        .processes("acme", &nucleos::contract::ProcessQuery::default())
        .await?;
    assert!(
        page.processes.iter().any(|info| info.0 == id),
        "a process being waited on is still a process: {:?}",
        page.processes
    );

    waiter.await.unwrap()?;
    Ok(())
}
