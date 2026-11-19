//! Two policies, one answer: the deployment's, and the tenant's.
//!
//! A tenant may restrict its own users and never lighten what the deployment decided. That is the same
//! rule `config.tools` and `config.sandbox` already follow — a narrower scope takes room away and never
//! adds it — applied to the one thing that decides everything else.
//!
//! The composition is deliberately not "the tenant's document replaces the deployment's": a tenant that
//! wrote an empty policy would then allow everything the deployment forbids. Instead both documents
//! answer the same question, and the stricter answer wins:
//!
//! | deployment | tenant | result |
//! | --- | --- | --- |
//! | allow | deny | **deny** |
//! | deny | allow | **deny** |
//! | allow | ask | **ask** |
//! | allow | (no tenant policy) | allow — a tenant without one inherits the deployment's |
//!
//! The ordering is the one the policy document already uses internally: refusing beats asking, and
//! asking beats allowing. A tenant can therefore turn an allow into a question, or a question into a
//! refusal, and cannot do the reverse.

use crate::document::Document;
use crate::ruleset::Ruleset;
use async_trait::async_trait;
use nucleos::adapter::{Policy, PolicyRequest, Verdict};
use nucleos::error::Result;
use std::collections::BTreeMap;

/// The deployment's policy, and one per tenant that wrote its own.
#[derive(Debug, Clone, Default)]
pub struct Layered {
    system: Option<Ruleset>,
    tenants: BTreeMap<String, Ruleset>,
}

impl Layered {
    /// A deployment policy with no tenant overrides.
    pub fn system(document: Document) -> Result<Self> {
        Ok(Self {
            system: Some(Ruleset::new(document)?),
            tenants: BTreeMap::new(),
        })
    }

    /// Add or replace one tenant's policy.
    pub fn with_tenant(mut self, tenant: impl Into<String>, document: Document) -> Result<Self> {
        self.tenants.insert(tenant.into(), Ruleset::new(document)?);
        Ok(self)
    }

    /// The tenant policies in force, for a `config` listing.
    #[must_use]
    pub fn tenants(&self) -> Vec<&str> {
        self.tenants.keys().map(String::as_str).collect()
    }

    /// Whether a tenant wrote a policy of its own.
    #[must_use]
    pub fn has_tenant(&self, tenant: &str) -> bool {
        self.tenants.contains_key(tenant)
    }
}

#[async_trait]
impl Policy for Layered {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        let tenant = request.principal.tenant.clone();
        let Some(systemial) = self.system.as_ref() else {
            // No deployment policy at all: the only honest answer is the strict one. A kernel is built
            // with a policy or not at all, so this is the shape of a bug rather than a decision.
            return Verdict::Deny {
                code: nucleos::contract::ErrorCode::Denied,
                reason: "no policy is loaded".to_owned(),
            };
        };

        let base = systemial.check(clone_request(&request)).await;
        let Some(tenant_policy) = self.tenants.get(&tenant) else {
            return base;
        };
        stricter(base, tenant_policy.check(request).await)
    }
}

/// A second request over the same values, so both policies can be asked without consuming either.
///
/// `PolicyRequest` holds borrows, and `check` takes it by value; a two-pass evaluation needs the same
/// borrows twice. Every field is `Copy` or a reference, so this costs nothing.
fn clone_request<'a>(request: &PolicyRequest<'a>) -> PolicyRequest<'a> {
    PolicyRequest {
        principal: request.principal,
        session: request.session,
        subject: request.subject,
    }
}

/// The stricter of two verdicts.
fn stricter(left: Verdict, right: Verdict) -> Verdict {
    match (&left, &right) {
        // A refusal is final whichever side made it, and keeps its own reason: the message a model
        // reads should be the one that actually applied.
        (Verdict::Deny { .. }, _) => left,
        (_, Verdict::Deny { .. }) => right,
        (Verdict::AskHuman { .. }, _) => left,
        (_, Verdict::AskHuman { .. }) => right,
        _ => Verdict::Allow,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nucleos::adapter::{Gated, Verdict};
    use nucleos::contract::{Principal, SessionId, ToolCall};

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: "{}".to_owned(),
            meta: Default::default(),
        }
    }

    fn document(json: &str) -> Document {
        Document::parse(json).unwrap()
    }

    async fn check(layered: &Layered, tenant: &str) -> Verdict {
        let principal = Principal {
            tenant: tenant.to_owned(),
            subject: "alice".to_owned(),
        };
        let session = SessionId::new("s1");
        let call = call();
        layered
            .check(PolicyRequest {
                principal: &principal,
                session: Some(&session),
                subject: Gated::Tool { call: &call },
            })
            .await
    }

    const ALLOW_READ: &str = r#"{"default":"deny","rules":[{"decision":"allow","tool":"read_*"}]}"#;
    const DENY_READ: &str = r#"{"default":"deny","rules":[{"decision":"deny","tool":"read_*"}]}"#;
    const ASK_READ: &str =
        r#"{"default":"deny","rules":[{"decision":"ask","tool":"read_*","question":"read it?"}]}"#;

    #[tokio::test]
    async fn a_tenant_without_a_policy_inherits_the_deployments() {
        let layered = Layered::system(document(ALLOW_READ)).unwrap();
        assert_eq!(check(&layered, "acme").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn a_tenant_can_turn_an_allow_into_a_refusal() {
        let layered = Layered::system(document(ALLOW_READ))
            .unwrap()
            .with_tenant("acme", document(DENY_READ))
            .unwrap();

        assert!(matches!(
            check(&layered, "acme").await,
            Verdict::Deny { .. }
        ));
        // And only its own tenant: the other one is untouched.
        assert_eq!(check(&layered, "other").await, Verdict::Allow);
    }

    #[tokio::test]
    async fn a_tenant_cannot_lighten_what_the_deployment_refuses() {
        // The failure this prevents: a tenant writing a permissive policy of its own and escaping the
        // deployment's rules. Both answer, and the stricter answer is the answer.
        let layered = Layered::system(document(DENY_READ))
            .unwrap()
            .with_tenant("acme", document(ALLOW_READ))
            .unwrap();
        assert!(matches!(
            check(&layered, "acme").await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn an_ask_beats_an_allow_and_loses_to_a_refusal() {
        let asking = Layered::system(document(ALLOW_READ))
            .unwrap()
            .with_tenant("acme", document(ASK_READ))
            .unwrap();
        assert!(matches!(
            check(&asking, "acme").await,
            Verdict::AskHuman { .. }
        ));

        let refused = Layered::system(document(DENY_READ))
            .unwrap()
            .with_tenant("acme", document(ASK_READ))
            .unwrap();
        assert!(matches!(
            check(&refused, "acme").await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_tenant_may_only_change_its_own_configuration() {
        // `scope: "own"` compares against the caller, which is what lets a deployment delegate
        // self-service without letting one tenant edit another's settings.
        let layered = Layered::system(document(
            r#"{"default":"deny","rules":[{"decision":"allow","config":"*","scope":"own"}]}"#,
        ))
        .unwrap();

        let principal = Principal {
            tenant: "acme".to_owned(),
            subject: "admin".to_owned(),
        };
        for (scope, expected) in [
            (nucleos::adapter::ConfigScope::Tenant("acme"), true),
            (nucleos::adapter::ConfigScope::Tenant("other"), false),
            (nucleos::adapter::ConfigScope::System, false),
        ] {
            let verdict = layered
                .check(PolicyRequest {
                    principal: &principal,
                    session: None,
                    subject: Gated::Config {
                        scope,
                        subject: "policy",
                    },
                })
                .await;
            assert_eq!(matches!(verdict, Verdict::Allow), expected, "{scope:?}");
        }
    }
}
