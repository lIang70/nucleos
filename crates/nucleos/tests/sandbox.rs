//! The sandbox contract.
//!
//! Two things are being pinned here. First, that the policy is *session* state and survives a
//! checkpoint — a policy that resets on reload is not a policy. Second, that the only thing a
//! single command may say is "more": `WithAdditionalPermissions` widens, it never narrows.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use nucleos::contract::Routing;
use nucleos::contract::{
    AdditionalPermissions, Principal, SandboxPermissions, SandboxPolicy, SessionId, SessionState,
};
use nucleos::kernel::{Kernel, SessionSpec};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

#[test]
fn the_default_policy_is_useful_and_not_the_loosest() {
    let policy = SandboxPolicy::default();
    assert!(matches!(policy, SandboxPolicy::WorkspaceWrite { .. }));
    assert!(!policy.network_access(), "no network unless asked");
    assert_ne!(
        policy,
        SandboxPolicy::DangerFullAccess,
        "a default that needs no decision is a default somebody will regret"
    );
}

#[test]
fn only_full_access_implies_the_network() {
    assert!(SandboxPolicy::DangerFullAccess.network_access());
    assert!(!SandboxPolicy::read_only().network_access());
    assert!(
        !SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        }
        .network_access()
    );
    assert!(
        SandboxPolicy::ExternalSandbox {
            network_access: true,
        }
        .network_access()
    );
}

#[test]
fn widening_is_additive_on_the_wire() {
    // The wire shape is the contract's own serde shape, so this is the wire shape: a tagged
    // union where "more room" carries only what it adds.
    let permissions = SandboxPermissions::WithAdditionalPermissions {
        additional: AdditionalPermissions {
            writable_roots: vec!["/var/tmp".to_owned()],
            network_access: true,
        },
    };
    let encoded = serde_json::to_value(&permissions).unwrap();

    assert_eq!(encoded["kind"], "with_additional_permissions");
    assert_eq!(encoded["additional"]["writable_roots"][0], "/var/tmp");
    assert_eq!(encoded["additional"]["network_access"], true);
    assert!(
        encoded["additional"].get("read_only").is_none(),
        "there is nothing here to narrow with"
    );

    let back: SandboxPermissions = serde_json::from_value(encoded).unwrap();
    assert_eq!(back, permissions);
}

#[tokio::test]
async fn the_policy_is_session_state_and_survives_a_checkpoint() -> nucleos::Result<()> {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()?;

    let id = SessionId::new("s1");
    let spec = SessionSpec {
        workspace: Some("/work".to_owned()),
        sandbox: SandboxPolicy::read_only(),
        credit_tokens: 1_000,
        ..SessionSpec::default()
    };
    kernel.open(&principal(), id.clone(), spec).await?;

    // A command's policy cannot be a request field, so it has to be here — and it has to come
    // back with the session, or a reload quietly loosens it to the default.
    let snapshot = store.snapshot(&id).await?.unwrap();
    assert_eq!(snapshot.state, SessionState::Open);
    assert_eq!(snapshot.workspace.as_deref(), Some("/work"));
    assert_eq!(snapshot.sandbox, SandboxPolicy::read_only());
    assert!(!snapshot.sandbox.network_access());

    let reloaded = kernel.load(&principal(), &id).await?;
    assert_eq!(reloaded.sandbox(), &SandboxPolicy::read_only());
    assert_eq!(reloaded.workspace().map(String::as_str), Some("/work"));
    Ok(())
}

// ---------------------------------------------------------------------------
// The mechanism
//
// A policy that nothing enforces is a document. These run a real command under a real sandbox and
// look at whether the file it wrote exists.
// ---------------------------------------------------------------------------

/// A temporary directory to use as a workspace, outside the system temp root that the profile does
/// not allow.
fn workspace(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("nucleos-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Run one command and wait for it, returning whether it succeeded.
async fn run(
    workspace: Option<&str>,
    policy: SandboxPolicy,
    permissions: SandboxPermissions,
    argv: &[&str],
) -> nucleos::Result<bool> {
    Ok(run_with(workspace, policy, permissions, argv).await?.0)
}

/// The same, with the event log: a sandbox that cannot say what it did is a sandbox nobody can
/// debug.
async fn run_with(
    workspace: Option<&str>,
    policy: SandboxPolicy,
    permissions: SandboxPermissions,
    argv: &[&str],
) -> nucleos::Result<(bool, Vec<nucleos::contract::Event>)> {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()?;

    let session = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            SessionSpec {
                workspace: workspace.map(ToString::to_string),
                sandbox: policy,
                credit_tokens: 1_000,
                process: Some(nucleos::contract::ProcessSpec {
                    argv: argv.iter().map(|part| (*part).to_string()).collect(),
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    permissions,
                }),
                ..SessionSpec::default()
            },
        )
        .await?;
    let output = kernel.wait(&session).await?;
    let events = store.events_since(session.id(), 0).await?;
    Ok((output.exit_code == 0, events))
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_workspace_write_policy_writes_in_the_workspace_and_nowhere_else() {
    let directory = workspace("write");
    let inside = directory.join("inside.txt");
    let outside = std::env::temp_dir().join(format!("nucleos-outside-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    let ok = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &[
            "/bin/sh",
            "-c",
            &format!("printf hi > {}", inside.display()),
        ],
    )
    .await
    .unwrap();
    assert!(ok, "a write inside the workspace is the point of the policy");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hi");

    // The same write one directory up: the profile has no rule for it, so the kernel refuses it.
    let denied = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &[
            "/bin/sh",
            "-c",
            &format!("printf hi > {}", outside.display()),
        ],
    )
    .await
    .unwrap();
    assert!(!denied, "a write outside the workspace must fail");
    assert!(!outside.exists(), "and must not have happened");

    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_read_only_policy_refuses_the_workspace_write_too() {
    let directory = workspace("readonly");
    let target = directory.join("should-not-exist.txt");

    let ok = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::read_only(),
        SandboxPermissions::UseDefault,
        &[
            "/bin/sh",
            "-c",
            &format!("printf hi > {}", target.display()),
        ],
    )
    .await
    .unwrap();

    assert!(!ok, "read-only means read only");
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_command_that_asks_for_more_gets_more() {
    let directory = workspace("widen");
    let outside = workspace("widen-outside");
    let target = outside.join("allowed.txt");

    let denied = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &[
            "/bin/sh",
            "-c",
            &format!("printf hi > {}", target.display()),
        ],
    )
    .await
    .unwrap();
    assert!(!denied, "not allowed by the session policy");

    // The same command, asking for the one root it needs. Widening is additive and per-command.
    let allowed = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::WithAdditionalPermissions {
            additional: AdditionalPermissions {
                writable_roots: vec![outside.to_str().unwrap().to_owned()],
                network_access: false,
            },
        },
        &[
            "/bin/sh",
            "-c",
            &format!("printf hi > {}", target.display()),
        ],
    )
    .await
    .unwrap();
    assert!(allowed, "the widened root is writable for this command");

    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_confining_policy_that_cannot_be_enforced_is_refused_not_ignored() {
    // Nothing to prove about the command: the point is that no unconfined process was started for
    // a policy that claims confinement. On macOS this cannot happen, so it documents the contract
    // rather than exercising a branch.
    let directory = workspace("enforced");
    let ok = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::read_only(),
        SandboxPermissions::UseDefault,
        &["/bin/echo", "hello"],
    )
    .await
    .unwrap();
    assert!(ok, "a read-only policy still runs a command that only reads");
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn a_confining_policy_is_refused_on_a_platform_without_a_mechanism() {
    // Honest refusal beats a sandbox that is not there.
    let error = run(
        None,
        SandboxPolicy::read_only(),
        SandboxPermissions::UseDefault,
        &["/bin/echo", "hello"],
    )
    .await
    .expect_err("no mechanism, no promise");
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Unavailable);
}


// ---------------------------------------------------------------------------
// Network
//
// A port on the loopback interface is the one thing a test can assert without the internet: the
// connection either happens or the sandbox refuses it before it reaches the kernel.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn connect_argv(port: u16) -> Vec<String> {
    vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
    ]
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_policy_without_network_cannot_reach_a_local_port() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let argv = connect_argv(port);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let directory = workspace("net-deny");

    let denied = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &argv,
    )
    .await
    .unwrap();
    assert!(!denied, "no rule allows the connection, so it must not happen");

    // The same command with the network allowed, so the refusal above is the sandbox and not a
    // broken listener.
    let allowed = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: true,
        },
        SandboxPermissions::UseDefault,
        &argv,
    )
    .await
    .unwrap();
    assert!(allowed, "with network_access the connection is made");

    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn one_command_may_ask_for_the_network_and_the_session_is_unchanged() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let argv = connect_argv(port);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let directory = workspace("net-widen");
    let policy = SandboxPolicy::WorkspaceWrite {
        extra_writable_roots: Vec::new(),
        network_access: false,
    };

    // Widening is for this command only, and it is asked for rather than assumed.
    let allowed = run(
        Some(directory.to_str().unwrap()),
        policy.clone(),
        SandboxPermissions::WithAdditionalPermissions {
            additional: AdditionalPermissions {
                writable_roots: Vec::new(),
                network_access: true,
            },
        },
        &argv,
    )
    .await
    .unwrap();
    assert!(allowed, "the command asked, and the request was additive");

    // And a second command under the same policy is back to no network.
    let refused = run(
        Some(directory.to_str().unwrap()),
        policy,
        SandboxPermissions::UseDefault,
        &argv,
    )
    .await
    .unwrap();
    assert!(!refused, "a widening must not outlive the command that asked");

    let _ = std::fs::remove_dir_all(&directory);
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_log_says_which_mechanism_ran_the_command_and_with_what_profile() {
    let directory = workspace("record");
    let root = directory.to_str().unwrap().to_owned();
    let (ok, events) = run_with(
        Some(&root),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &["/bin/echo", "hi"],
    )
    .await
    .unwrap();
    assert!(ok);

    let started = events
        .iter()
        .find_map(|event| match &event.kind {
            nucleos::contract::EventKind::ExecStarted {
                mechanism, profile, ..
            } => Some((mechanism.clone(), profile.clone())),
            _ => None,
        })
        .expect("the start is recorded");

    assert_eq!(started.0.as_deref(), Some("seatbelt"));
    let profile = started.1.expect("seatbelt records the profile it was given");
    // What a reader needs: that it was deny-by-default, and which root it was allowed to write.
    assert!(profile.contains("(deny default)"), "{profile}");
    assert!(profile.contains(&root), "{profile}");
    assert!(!profile.contains("(allow network*)"), "{profile}");

    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn an_unconfined_command_says_so_rather_than_leaving_it_to_inference() {
    // `danger_full_access` is a decision, and the log records it as one: no mechanism, no profile.
    let directory = workspace("record-open");
    let root = directory.to_str().unwrap().to_owned();
    let (ok, events) = run_with(
        Some(&root),
        SandboxPolicy::DangerFullAccess,
        SandboxPermissions::UseDefault,
        &["/bin/echo", "hi"],
    )
    .await
    .unwrap();
    assert!(ok);

    let started = events
        .iter()
        .find_map(|event| match &event.kind {
            nucleos::contract::EventKind::ExecStarted {
                mechanism, profile, ..
            } => Some((mechanism.clone(), profile.clone())),
            _ => None,
        })
        .expect("the start is recorded");
    assert_eq!(started.0, None, "nothing confined it");
    assert_eq!(started.1, None);

    let _ = std::fs::remove_dir_all(&directory);
}

// ---------------------------------------------------------------------------
// Narrowing
//
// A session is allowed to give up room while it runs. The rule is the one tools already follow:
// narrowing is a configuration change, widening is an escalation, and an escalation is decided
// about one command by the gate.
// ---------------------------------------------------------------------------

#[test]
fn the_order_is_by_what_a_process_may_touch() {
    let writable = SandboxPolicy::WorkspaceWrite {
        extra_writable_roots: vec!["/srv/cache".to_owned()],
        network_access: true,
    };
    let narrower = SandboxPolicy::WorkspaceWrite {
        extra_writable_roots: Vec::new(),
        network_access: false,
    };

    assert!(writable.narrows_to(&narrower), "fewer roots and no network");
    assert!(writable.narrows_to(&SandboxPolicy::read_only()), "no writes at all");
    assert!(
        SandboxPolicy::DangerFullAccess.narrows_to(&writable),
        "anything is narrower than nothing"
    );

    assert!(!narrower.narrows_to(&writable), "adding room back is widening");
    assert!(
        !SandboxPolicy::read_only().narrows_to(&narrower),
        "a read-only session may not start writing"
    );
    assert!(
        !writable.narrows_to(&SandboxPolicy::DangerFullAccess),
        "nothing narrows into full access"
    );
    assert!(
        !writable.narrows_to(&SandboxPolicy::ExternalSandbox {
            network_access: false
        }),
        "leaving the kernel's control is not a narrowing"
    );
}

#[test]
fn a_network_flag_only_turns_off() {
    let with = SandboxPolicy::ReadOnly {
        network_access: true,
    };
    let without = SandboxPolicy::read_only();
    assert!(with.narrows_to(&without));
    assert!(!without.narrows_to(&with));
}

#[tokio::test]
async fn a_session_may_narrow_its_sandbox_and_may_not_widen_it() {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
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

    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                workspace: Some("/srv/ws".to_owned()),
                sandbox: SandboxPolicy::WorkspaceWrite {
                    extra_writable_roots: vec!["/srv/cache".to_owned()],
                    network_access: true,
                },
                credit_tokens: 1_000,
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    // Narrowing applies, and it is recorded like every other change.
    let changed = kernel
        .configure(
            &mut session,
            nucleos::contract::ConfigPatch {
                sandbox: Some(SandboxPolicy::read_only()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(changed, ["sandbox"]);
    assert_eq!(session.sandbox(), &SandboxPolicy::read_only());

    // Widening is refused, and the refusal names the difference rather than the field.
    let widened = kernel
        .configure(
            &mut session,
            nucleos::contract::ConfigPatch {
                sandbox: Some(SandboxPolicy::WorkspaceWrite {
                    extra_writable_roots: vec!["/srv/cache".to_owned()],
                    network_access: true,
                }),
                ..Default::default()
            },
        )
        .await;
    let error = widened.expect_err("widening is an escalation, not a configuration change");
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Denied);
    assert!(error.reason().contains("escalation"), "{error:?}");
    assert_eq!(session.sandbox(), &SandboxPolicy::read_only(), "unchanged");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn narrowing_is_enforced_not_just_recorded() {
    // A session that gives up its workspace write really cannot write afterwards: the policy the
    // mechanism reads is the one in force, not the one it was created with.
    let directory = workspace("narrow");
    let target = directory.join("after-narrowing.txt");
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
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

    let mut session = kernel
        .spawn(
            &principal(),
            SessionId::new("p1"),
            SessionSpec {
                workspace: Some(directory.to_str().unwrap().to_owned()),
                sandbox: SandboxPolicy::WorkspaceWrite {
                    extra_writable_roots: Vec::new(),
                    network_access: false,
                },
                credit_tokens: 1_000,
                process: Some(nucleos::contract::ProcessSpec {
                    argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), "true".to_owned()],
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    permissions: SandboxPermissions::UseDefault,
                }),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    kernel.wait(&session).await.unwrap();

    kernel
        .configure(
            &mut session,
            nucleos::contract::ConfigPatch {
                sandbox: Some(SandboxPolicy::read_only()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Now ask for a write through a second process in the same session.
    let second = kernel
        .spawn(
            &principal(),
            SessionId::new("p2"),
            SessionSpec {
                workspace: Some(directory.to_str().unwrap().to_owned()),
                sandbox: session.sandbox().clone(),
                credit_tokens: 1_000,
                process: Some(nucleos::contract::ProcessSpec {
                    argv: vec![
                        "/bin/sh".to_owned(),
                        "-c".to_owned(),
                        format!("printf hi > {}", target.display()),
                    ],
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    permissions: SandboxPermissions::UseDefault,
                }),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    let output = kernel.wait(&second).await.unwrap();
    assert_ne!(output.exit_code, 0, "read-only means read only");
    assert!(!target.exists());

    let _ = std::fs::remove_dir_all(&directory);
}

// ---------------------------------------------------------------------------
// The same mechanism on Linux, through bubblewrap
//
// Written but not exercised on macOS. CI is Linux, so these run there — and skip *loudly* when the
// kernel forbids unprivileged user namespaces, because a silent skip is a test that passes by
// accident.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn bwrap_usable() -> bool {
    // The same probe a person would run by hand: can this kernel start a namespaced command at all?
    let probe = std::process::Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
        .status();
    match probe {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn require_bwrap(test: &str) -> bool {
    if bwrap_usable() {
        return true;
    }
    eprintln!(
        "skipping {test}: bubblewrap cannot start here (missing, or unprivileged user namespaces \
         are disabled). The mechanism is unverified in this environment rather than passing."
    );
    false
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_workspace_write_policy_is_enforced_by_bubblewrap() {
    if !require_bwrap("a_workspace_write_policy_is_enforced_by_bubblewrap") {
        return;
    }
    let directory = workspace("bwrap-write");
    let inside = directory.join("inside.txt");
    let outside = std::env::temp_dir().join(format!("nucleos-bwrap-out-{}", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    let ok = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &["/bin/sh", "-c", &format!("printf hi > {}", inside.display())],
    )
    .await
    .unwrap();
    assert!(ok, "a write inside the workspace is the point of the policy");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hi");

    let denied = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        },
        SandboxPermissions::UseDefault,
        &["/bin/sh", "-c", &format!("printf hi > {}", outside.display())],
    )
    .await
    .unwrap();
    assert!(!denied, "a write outside the workspace must fail");
    assert!(!outside.exists());

    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_read_only_policy_refuses_every_write_on_linux_too() {
    if !require_bwrap("a_read_only_policy_refuses_every_write_on_linux_too") {
        return;
    }
    let directory = workspace("bwrap-readonly");
    let target = directory.join("should-not-exist.txt");

    let ok = run(
        Some(directory.to_str().unwrap()),
        SandboxPolicy::read_only(),
        SandboxPermissions::UseDefault,
        &["/bin/sh", "-c", &format!("printf hi > {}", target.display())],
    )
    .await
    .unwrap();

    assert!(!ok, "read-only means read only");
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_log_records_bubblewrap_as_the_mechanism() {
    if !require_bwrap("the_log_records_bubblewrap_as_the_mechanism") {
        return;
    }
    let directory = workspace("bwrap-record");
    let root = directory.to_str().unwrap().to_owned();
    let (ok, events) = run_with(
        Some(&root),
        SandboxPolicy::read_only(),
        SandboxPermissions::UseDefault,
        &["/bin/echo", "hi"],
    )
    .await
    .unwrap();
    assert!(ok);

    let started = events
        .iter()
        .find_map(|event| match &event.kind {
            nucleos::contract::EventKind::ExecStarted {
                mechanism, profile, ..
            } => Some((mechanism.clone(), profile.clone())),
            _ => None,
        })
        .expect("the start is recorded");
    assert_eq!(started.0.as_deref(), Some("bubblewrap"));
    let command = started.1.expect("bwrap records the command line it was given");
    assert!(command.contains("--unshare-all"), "{command}");
    assert!(command.contains("--ro-bind"), "{command}");

    let _ = std::fs::remove_dir_all(&directory);
}
