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
    Ok(output.exit_code == 0)
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
