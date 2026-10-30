//! Configuration changes go through the policy, and a change that needs a human is refused.
//!
//! Configuration is a privilege before it is a convenience: whoever may add a model may point every
//! session's traffic at their own server, and whoever may replace the policy may replace it with one
//! that says yes. These tests pin the two properties that make the gate worth having: a tool rule
//! grants nothing over configuration, and an `ask` answer becomes a refusal rather than a wait.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, ConfigScope, DenyAll, Policy, PolicyRequest, Verdict};
use nucleos::contract::Principal;
use nucleos::kernel::Kernel;
use nucleos_policy::{Document, Ruleset};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel(policy: Arc<dyn Policy>) -> Kernel {
    Kernel::builder()
        .store(Arc::new(
            nucleos_sqlite::SqliteStore::in_memory().unwrap(),
        ))
        .policy(policy)
        .build()
        .unwrap()
}

fn rules(json: &str) -> Arc<Ruleset> {
    let document: Document = serde_json::from_str(json).unwrap();
    document.validate().unwrap();
    Arc::new(Ruleset::new(document).unwrap())
}

#[tokio::test]
async fn a_permissive_policy_lets_an_administrator_change_configuration() {
    let kernel = kernel(Arc::new(AllowAll));
    assert_eq!(
        kernel
            .check_config(&principal(), ConfigScope::System, "models")
            .await
            .unwrap(),
        Verdict::Allow
    );
}

#[tokio::test]
async fn a_refusing_policy_refuses_configuration_too() {
    let kernel = kernel(Arc::new(DenyAll));
    let verdict = kernel
        .check_config(&principal(), ConfigScope::System, "policy")
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Deny { .. }), "{verdict:?}");
}

#[tokio::test]
async fn a_tool_rule_grants_no_authority_over_configuration() {
    // The deployment allowed every tool. That says nothing about who may add a model.
    let kernel = kernel(rules(r#"{"rules":[{"decision":"allow","tool":"*"}]}"#) as Arc<dyn Policy>);
    let verdict = kernel
        .check_config(&principal(), ConfigScope::System, "models")
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Deny { .. }), "{verdict:?}");
}

#[tokio::test]
async fn a_change_that_needs_a_human_is_refused_rather_than_waited_on() {
    // Configuration has no session to park, so an `ask` cannot become an answer later. Turning it
    // into a denial is the only honest translation: an unattended "ask" that expires into an
    // "allow" would be a gate that opens by itself.
    let kernel = kernel(
        rules(
            r#"{"rules":[{"decision":"ask","config":"policy","scope":"system",
                "question":"Replace the system policy?"}]}"#,
        ) as Arc<dyn Policy>,
    );
    let verdict = kernel
        .check_config(&principal(), ConfigScope::System, "policy")
        .await
        .unwrap();
    let Verdict::Deny { reason, .. } = verdict else {
        panic!("expected a refusal, got {verdict:?}");
    };
    assert!(reason.contains("needs a human"), "{reason}");
    assert!(
        reason.contains("administrator's credential"),
        "the refusal has to say what to do instead: {reason}"
    );
}

#[tokio::test]
async fn an_administrator_may_be_allowed_one_subject_and_not_another() {
    // The ordinary shape: models and sources are delegated, the policy is not.
    let kernel = kernel(
        rules(
            r#"{"default":"deny","rules":[
                {"decision":"allow","config":"models","scope":"system"},
                {"decision":"allow","config":"sources","scope":"system"}
            ]}"#,
        ) as Arc<dyn Policy>,
    );

    for allowed in ["models", "sources"] {
        assert_eq!(
            kernel
                .check_config(&principal(), ConfigScope::System, allowed)
                .await
                .unwrap(),
            Verdict::Allow,
            "{allowed}"
        );
    }
    for refused in ["policy", "tokens", "admin_keys"] {
        assert!(
            matches!(
                kernel
                    .check_config(&principal(), ConfigScope::System, refused)
                    .await
                    .unwrap(),
                Verdict::Deny { .. }
            ),
            "{refused} must need a different authority"
        );
    }
}

#[tokio::test]
async fn a_tenant_may_be_scoped_to_its_own_configuration() {
    let ruleset = rules(
        r#"{"default":"deny","rules":[
            {"decision":"allow","config":"*","scope":"tenant"}
        ]}"#,
    );
    let kernel = kernel(ruleset as Arc<dyn Policy>);

    assert_eq!(
        kernel
            .check_config(&principal(), ConfigScope::Tenant("acme"), "policy")
            .await
            .unwrap(),
        Verdict::Allow
    );
    // The system scope is not reached by a rule written for tenants.
    assert!(matches!(
        kernel
            .check_config(&principal(), ConfigScope::System, "policy")
            .await
            .unwrap(),
        Verdict::Deny { .. }
    ));
}

/// A policy that only records what it was asked, to prove the request carries the scope.
struct Recording(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl Policy for Recording {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        if let nucleos::adapter::Gated::Config { scope, subject } = request.subject {
            self.0
                .lock()
                .unwrap()
                .push(format!("{scope}:{subject}"));
        }
        Verdict::Allow
    }
}

#[tokio::test]
async fn the_gate_tells_policy_which_scope_and_which_subject() {
    // A policy that cannot see the scope cannot distinguish a tenant narrowing its own settings
    // from a tenant editing the deployment's.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kernel = kernel(Arc::new(Recording(Arc::clone(&seen))));

    kernel
        .check_config(&principal(), ConfigScope::System, "models")
        .await
        .unwrap();
    kernel
        .check_config(&principal(), ConfigScope::Tenant("acme"), "policy")
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        ["system:models".to_owned(), "tenant/acme:policy".to_owned()]
    );
}
