//! The sandbox contract.
//!
//! Two things are being pinned here. First, that the policy is *session* state and survives a
//! checkpoint — a policy that resets on reload is not a policy. Second, that the only thing a
//! single command may say is "more": `WithAdditionalPermissions` widens, it never narrows.

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{AllowAll, FakeModel, FixedClock, MemoryStore, Store};
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
