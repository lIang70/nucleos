//! Answering: which rule applies, and what it decides.

use crate::document::{Decision, Document, Rule, Unmatched};
use async_trait::async_trait;
use nucleos::adapter::{Gated, Policy, PolicyRequest, Verdict};
use nucleos::contract::{ErrorCode, PermissionKind, PermissionOption};
use nucleos::error::Result;
use std::path::Path;

/// A document, ready to answer.
#[derive(Debug, Clone, Default)]
pub struct Ruleset {
    document: Document,
}

impl Ruleset {
    /// Compile a document.
    pub fn new(document: Document) -> Result<Self> {
        document.validate()?;
        Ok(Self { document })
    }

    /// Read one.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            document: Document::load(path)?,
        })
    }

    /// The document in force, for a `describe` or a log line.
    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    fn decide(&self, request: &PolicyRequest<'_>) -> Verdict {
        // Deny first, then ask, then allow. The order is by decision so that moving a line in the
        // file cannot change an answer, and so that a mistake in a broad `allow` cannot widen past
        // a narrow `deny`.
        for decision in [Decision::Deny, Decision::Ask, Decision::Allow] {
            if let Some(rule) = self
                .document
                .rules
                .iter()
                .find(|rule| rule.decision == Some(decision) && matches(rule, request))
            {
                return verdict(decision, rule, request);
            }
        }
        match self.document.unmatched {
            Unmatched::Allow => Verdict::Allow,
            Unmatched::Deny => Verdict::Deny {
                code: ErrorCode::Denied,
                reason: format!("no policy rule allows {}", describe(&request.subject)),
            },
        }
    }
}

#[async_trait]
impl Policy for Ruleset {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        self.decide(&request)
    }
}

fn verdict(decision: Decision, rule: &Rule, request: &PolicyRequest<'_>) -> Verdict {
    tracing::debug!(
        event = "policy_decided",
        decision = ?decision,
        subject = %describe(&request.subject),
        tenant = %request.principal.tenant,
        subject_id = %request.principal.subject,
        reason = rule.reason.as_deref().unwrap_or(""),
    );
    match decision {
        Decision::Allow => Verdict::Allow,
        Decision::Deny => Verdict::Deny {
            code: ErrorCode::Denied,
            reason: rule
                .reason
                .clone()
                .unwrap_or_else(|| format!("{} is denied by policy", describe(&request.subject))),
        },
        Decision::Ask => Verdict::AskHuman {
            question: rule
                .question
                .clone()
                .unwrap_or_else(|| format!("{} needs approval", describe(&request.subject))),
            // Allow once and refuse, and no "always" option: remembering a decision for the whole
            // session is the host's call, not something a rule can promise on its behalf.
            options: vec![
                PermissionOption {
                    id: "allow_once".to_owned(),
                    kind: PermissionKind::AllowOnce,
                    label: "Allow this once".to_owned(),
                },
                PermissionOption {
                    id: "reject".to_owned(),
                    kind: PermissionKind::Reject,
                    label: "Refuse".to_owned(),
                },
            ],
        },
    }
}

/// Whether a rule applies to this request, both in subject and in scope.
fn matches(rule: &Rule, request: &PolicyRequest<'_>) -> bool {
    if let Some(tenant) = &rule.tenant
        && *tenant != request.principal.tenant
    {
        return false;
    }
    if let Some(subject) = &rule.subject
        && *subject != request.principal.subject
    {
        return false;
    }

    match &request.subject {
        // A session's isolation: the kind, the network, and where it may write.
        Gated::Sandbox { policy, workspace } => {
            let Some(pattern) = &rule.sandbox else {
                return false;
            };
            if !glob(pattern, policy.kind()) {
                return false;
            }
            // Network is denied unless the rule says otherwise: it is not part of the kind, so a rule
            // that allowed `workspace_write` would otherwise also allow it with the network on.
            if policy.network_access() && rule.network != Some(true) {
                return false;
            }
            if let Some(prefix) = &rule.workspace_prefix
                && policy
                    .writable_roots(*workspace)
                    .iter()
                    .any(|root| !root.starts_with(prefix.as_str()))
            {
                return false;
            }
            true
        }
        Gated::Tool { call } => rule
            .tool
            .as_ref()
            .is_some_and(|pattern| glob(pattern, &call.name)),
        Gated::Model { model, .. } => rule
            .model
            .as_ref()
            .is_some_and(|pattern| glob(pattern, model)),
        // Configuration has its own rule field, so a tool rule cannot accidentally grant it.
        Gated::Config { scope, subject } => {
            let Some(pattern) = &rule.config else {
                return false;
            };
            if !glob(pattern, subject) {
                return false;
            }
            match (&rule.scope, scope) {
                (None, _) => true,
                (Some(wanted), nucleos::adapter::ConfigScope::System) => wanted == "system",
                (Some(wanted), nucleos::adapter::ConfigScope::Tenant(tenant)) => {
                    // `own` is the only scope value that compares against the caller rather than
                    // against the request: it is how a deployment says "an administrator of a tenant
                    // may change that tenant's settings, and nobody else's".
                    match wanted.as_str() {
                        "tenant" => true,
                        "own" => *tenant == request.principal.tenant,
                        // Both spellings the documents use: a bare tenant name, and `tenant/<id>` as
                        // `docs/config.md` writes it. The split form was documented and matched
                        // nothing, which is worse than not documenting it.
                        named => {
                            named == *tenant
                                || named
                                    .strip_prefix("tenant/")
                                    .is_some_and(|named| named == *tenant)
                        }
                    }
                }
            }
        }
        // A rule that names a program applies to commands only. `argv_prefix` narrows within it.
        Gated::Exec {
            argv, permissions, ..
        } => {
            // Widening is a second question, and a rule answers it only if it says so. Without this,
            // any caller that could name an allowed program could also widen its own sandbox — by
            // sending `require_escalated`, or by asking for extra writable roots and the network,
            // which the gate previously never looked at.
            let widening = match permissions {
                nucleos::contract::SandboxPermissions::RequireEscalated => true,
                nucleos::contract::SandboxPermissions::WithAdditionalPermissions { additional } => {
                    !additional.writable_roots.is_empty() || additional.network_access
                }
                nucleos::contract::SandboxPermissions::UseDefault => false,
            };
            if widening && rule.escalation != Some(true) {
                return false;
            }
            let Some(program) = &rule.program else {
                return false;
            };
            let Some(first) = argv.first() else {
                return false;
            };
            if basename(first) != program.as_str() {
                return false;
            }
            argv.len() >= rule.argv_prefix.len().saturating_add(1)
                && argv
                    .iter()
                    .skip(1)
                    .zip(rule.argv_prefix.iter())
                    .all(|(argument, expected)| argument == expected)
        }
    }
}

/// A name for a request, for a refusal a model or a human can act on.
fn describe(subject: &Gated<'_>) -> String {
    match subject {
        Gated::Tool { call } => format!("tool `{}`", call.name),
        Gated::Model { model, .. } => format!("model `{model}`"),
        Gated::Exec { argv, .. } => match argv.first() {
            Some(program) => format!("command `{}`", basename(program)),
            None => "an empty command".to_owned(),
        },
        Gated::Config { scope, subject } => format!("{scope} configuration `{subject}`"),
        Gated::Sandbox { policy, .. } => format!("a `{}` session", policy.kind()),
    }
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// `*` matches any run of characters, including none. Nothing else is special.
///
/// No `?`, no character classes, no escaping: every one of those is a feature a policy author will
/// eventually use to express something subtly different from what they meant, and the failure is
/// silent in the permissive direction.
fn glob(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = value;

    // The part before the first star anchors the beginning.
    let Some(first) = parts.first() else {
        return true;
    };
    if !rest.starts_with(first) {
        return false;
    }
    rest = &rest[first.len()..];

    // Everything between stars has to appear in order; the last part anchors the end.
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    let Some(last) = parts.last() else {
        return true;
    };
    if pattern.ends_with('*') {
        return true;
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nucleos::contract::{
        Principal, Purpose, SandboxPermissions, SandboxPolicy, SessionId, ToolCall,
    };
    use std::collections::BTreeMap;

    fn principal() -> Principal {
        Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: name.to_owned(),
            arguments_json: "{}".to_owned(),
            meta: BTreeMap::new(),
        }
    }

    async fn check<'a>(document: Document, subject: Gated<'a>) -> Verdict {
        let principal = principal();
        let ruleset = Ruleset::new(document).unwrap();
        let session = SessionId::new("s1");
        ruleset
            .check(PolicyRequest {
                principal: &principal,
                session: Some(&session),
                subject,
            })
            .await
    }

    #[tokio::test]
    async fn nothing_matches_so_nothing_runs() {
        let read = call("read_file");
        let verdict = check(Document::default(), Gated::Tool { call: &read }).await;
        assert!(
            matches!(verdict, Verdict::Deny { .. }),
            "an empty policy is a policy, and it denies: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn a_default_of_allow_has_to_be_written_down() {
        let document = Document {
            unmatched: Unmatched::Allow,
            ..Document::default()
        };
        let anything = call("anything");
        assert_eq!(
            check(document, Gated::Tool { call: &anything }).await,
            Verdict::Allow
        );
    }

    #[tokio::test]
    async fn deny_wins_over_allow_whatever_the_file_order() {
        let document = Document {
            unmatched: Unmatched::Deny,
            rules: vec![
                Rule {
                    decision: Some(Decision::Allow),
                    tool: Some("*".to_owned()),
                    ..Rule::default()
                },
                Rule {
                    decision: Some(Decision::Deny),
                    tool: Some("exec".to_owned()),
                    reason: Some("not on this host".to_owned()),
                    ..Rule::default()
                },
            ],
        };
        let exec = call("exec");
        let verdict = check(document, Gated::Tool { call: &exec }).await;

        // The allow rule is broader, and it does not matter: order is by decision, not by line.
        assert_eq!(
            verdict,
            Verdict::Deny {
                code: ErrorCode::Denied,
                reason: "not on this host".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_glob_matches_the_names_it_names() {
        let document = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                tool: Some("read_*".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        let read = call("read_file");
        assert_eq!(
            check(document.clone(), Gated::Tool { call: &read }).await,
            Verdict::Allow
        );
        let write = call("write_file");
        assert!(matches!(
            check(document, Gated::Tool { call: &write }).await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_rule_is_scoped_to_the_principal_it_names() {
        let document = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                tool: Some("read_file".to_owned()),
                tenant: Some("other".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        let read = call("read_file");
        assert!(matches!(
            check(document, Gated::Tool { call: &read }).await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_command_is_matched_by_program_and_arguments_not_by_text() {
        let document = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                program: Some("ls".to_owned()),
                argv_prefix: vec!["-la".to_owned()],
                ..Rule::default()
            }],
            ..Document::default()
        };
        let policy = SandboxPolicy::default();
        let permissions = SandboxPermissions::UseDefault;
        let argv = |parts: &[&str]| -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        };

        let allowed = argv(&["/bin/ls", "-la"]);
        assert_eq!(
            check(
                document.clone(),
                Gated::Exec {
                    argv: &allowed,
                    policy: &policy,
                    permissions: &permissions,
                }
            )
            .await,
            Verdict::Allow,
            "the basename is what a rule names"
        );

        // A different program that happens to be spelled the same way as an argument is not it.
        let other = argv(&["ls; rm", "-la"]);
        assert!(matches!(
            check(
                document,
                Gated::Exec {
                    argv: &other,
                    policy: &policy,
                    permissions: &permissions,
                }
            )
            .await,
            Verdict::Deny { .. }
        ));
    }

    /// A rule that allows a program does not allow running it outside the sandbox.
    ///
    /// These are two questions, and the second one used to be answered by the first: the matcher looked
    /// at the program and never at `permissions`, so a caller that could name an allowed program could
    /// also remove its own confinement by setting `require_escalated` in the request.
    /// Both spellings of "this tenant" work: a bare name and the `tenant/<id>` form the docs use.
    #[tokio::test]
    async fn a_tenant_scope_matches_both_spellings() {
        let argv = vec!["ls".to_owned()];
        for scope in ["tenant/acme"] {
            let document = Document {
                rules: vec![Rule {
                    decision: Some(Decision::Allow),
                    config: Some("*".to_owned()),
                    scope: Some(scope.to_owned()),
                    tenant: Some("acme".to_owned()),
                    ..Rule::default()
                }],
                ..Document::default()
            };
            document.validate().unwrap_or_else(|error| {
                panic!("{scope:?} is a scope the document advertises: {error:?}")
            });
            assert_eq!(
                check(
                    document.clone(),
                    Gated::Config {
                        scope: nucleos::adapter::ConfigScope::Tenant("acme"),
                        subject: "policy",
                    }
                )
                .await,
                Verdict::Allow,
                "{scope:?}"
            );
            // And it does not reach another tenant.
            assert!(
                matches!(
                    check(
                        document,
                        Gated::Config {
                            scope: nucleos::adapter::ConfigScope::Tenant("other"),
                            subject: "policy",
                        }
                    )
                    .await,
                    Verdict::Deny { .. }
                ),
                "{scope:?} must not reach another tenant"
            );
        }
        let _ = argv;
    }

    #[tokio::test]
    async fn an_escalation_is_its_own_decision() {
        let policy = SandboxPolicy::default();
        let argv = vec!["/bin/echo".to_owned(), "hi".to_owned()];
        let allowed = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                program: Some("echo".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        let opting_in = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                program: Some("echo".to_owned()),
                escalation: Some(true),
                ..Rule::default()
            }],
            ..Document::default()
        };

        let default = SandboxPermissions::UseDefault;
        let escalated = SandboxPermissions::RequireEscalated;

        // The rule covers the command ...
        assert_eq!(
            check(
                allowed.clone(),
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &default,
                }
            )
            .await,
            Verdict::Allow
        );
        // ... and not the escalation, because it never said so.
        assert!(matches!(
            check(
                allowed,
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &escalated,
                }
            )
            .await,
            Verdict::Deny { .. }
        ));
        // A rule that opts in answers both.
        assert_eq!(
            check(
                opting_in,
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &escalated,
                }
            )
            .await,
            Verdict::Allow
        );
    }

    #[tokio::test]
    async fn an_ask_rule_parks_the_call_with_a_readable_question() {
        let document = Document {
            rules: vec![Rule {
                decision: Some(Decision::Ask),
                program: Some("git".to_owned()),
                question: Some("Push to the remote?".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        let policy = SandboxPolicy::default();
        let permissions = SandboxPermissions::UseDefault;
        let argv = vec!["git".to_owned(), "push".to_owned()];

        let verdict = check(
            document,
            Gated::Exec {
                argv: &argv,
                policy: &policy,
                permissions: &permissions,
            },
        )
        .await;
        let Verdict::AskHuman { question, options } = verdict else {
            panic!("expected a question, got {verdict:?}");
        };
        assert_eq!(question, "Push to the remote?");
        // No "always allow": remembering is the host's decision, not a rule's promise.
        assert!(
            options
                .iter()
                .all(|option| option.kind != PermissionKind::AllowAlways)
        );
    }

    #[tokio::test]
    async fn a_model_call_is_gated_too() {
        let document = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                model: Some("gpt-*".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        assert_eq!(
            check(
                document.clone(),
                Gated::Model {
                    model: "gpt-4o-mini",
                    purpose: Purpose::Answer,
                }
            )
            .await,
            Verdict::Allow
        );
        assert!(matches!(
            check(
                document,
                Gated::Model {
                    model: "claude",
                    purpose: Purpose::Answer,
                }
            )
            .await,
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn a_document_that_cannot_mean_what_it_says_is_refused() {
        let no_decision = Document {
            rules: vec![Rule {
                tool: Some("read_file".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        assert!(no_decision.validate().is_err());

        // An empty rule is a mistake, and a mistake must not become permission.
        let empty = Document {
            rules: vec![Rule {
                decision: Some(Decision::Allow),
                ..Rule::default()
            }],
            ..Document::default()
        };
        assert!(empty.validate().is_err());

        let unaskable = Document {
            rules: vec![Rule {
                decision: Some(Decision::Ask),
                tool: Some("exec".to_owned()),
                ..Rule::default()
            }],
            ..Document::default()
        };
        assert!(unaskable.validate().is_err());
    }

    #[test]
    fn the_shipped_example_is_a_document_that_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/policy.json");
        let document = Document::load(path).expect("the example policy loads");

        // And it is a policy, not decoration: something is allowed and something is refused.
        assert!(
            document
                .rules
                .iter()
                .any(|rule| rule.decision == Some(Decision::Allow))
        );
        assert!(
            document
                .rules
                .iter()
                .any(|rule| rule.decision == Some(Decision::Deny))
        );
    }

    #[test]
    fn a_glob_is_anchored_and_star_only() {
        assert!(glob("read_*", "read_file"));
        assert!(glob("*", "anything"));
        assert!(glob("*file", "read_file"));
        assert!(glob("read*file", "read_any_file"));
        assert!(glob("exact", "exact"));

        assert!(!glob("read_*", "write_file"));
        assert!(
            !glob("read_*", "xread_file"),
            "an unanchored match would pass"
        );
        assert!(!glob("exact", "exactly"), "no implicit prefix matching");
        assert!(!glob("read_?", "read_a"), "? is not a wildcard here");
    }
}

#[cfg(test)]
mod config_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nucleos::adapter::ConfigScope;
    use nucleos::contract::Principal;

    fn document(value: &str) -> Document {
        let document: Document = serde_json::from_str(value).unwrap();
        document.validate().unwrap();
        document
    }

    async fn check(document: Document, scope: ConfigScope<'_>, subject: &str) -> Verdict {
        let principal = Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        };
        Ruleset::new(document)
            .unwrap()
            .check(PolicyRequest {
                principal: &principal,
                session: None,
                subject: Gated::Config { scope, subject },
            })
            .await
    }

    #[tokio::test]
    async fn a_config_rule_matches_by_subject_and_scope() {
        let rules =
            document(r#"{"rules":[{"decision":"allow","config":"models","scope":"system"}]}"#);
        assert_eq!(
            check(rules.clone(), ConfigScope::System, "models").await,
            Verdict::Allow
        );
        // The subject is a glob: one rule can name the whole family.
        assert!(matches!(
            check(rules.clone(), ConfigScope::System, "policy").await,
            Verdict::Deny { .. }
        ));
        // And the scope narrows: a tenant caller cannot reach the system scope even when the
        // subject matches, because the request says which scope it is.
        assert!(matches!(
            check(rules, ConfigScope::Tenant("acme"), "models").await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_tool_rule_grants_nothing_over_configuration() {
        // The failure this prevents: a deployment that allowed `read_file` discovering it also
        // allowed adding a model, which points every session's traffic somewhere else.
        let rules = document(r#"{"rules":[{"decision":"allow","tool":"*"}]}"#);
        assert!(matches!(
            check(rules, ConfigScope::System, "models").await,
            Verdict::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn a_tenant_scope_rule_reaches_only_tenants() {
        let rules = document(r#"{"rules":[{"decision":"allow","config":"*","scope":"tenant"}]}"#);
        assert_eq!(
            check(rules.clone(), ConfigScope::Tenant("acme"), "policy").await,
            Verdict::Allow
        );
        assert!(matches!(
            check(rules, ConfigScope::System, "policy").await,
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn a_scope_without_a_config_subject_is_refused_at_load() {
        let parsed: Document =
            serde_json::from_str(r#"{"rules":[{"decision":"allow","scope":"system"}]}"#).unwrap();
        let error = parsed
            .validate()
            .expect_err("a scope on its own names nothing");
        assert!(error.reason().contains("scope"), "{error:?}");

        let unknown: Document = serde_json::from_str(
            r#"{"rules":[{"decision":"allow","config":"*","scope":"galaxy"}]}"#,
        )
        .unwrap();
        assert!(unknown.validate().is_err(), "only system and tenant exist");
    }

    #[test]
    fn a_config_request_is_named_in_a_refusal() {
        // A refusal has to say what was refused: "configuration models" is actionable, "denied" is
        // not.
        let described = describe(&Gated::Config {
            scope: ConfigScope::System,
            subject: "models",
        });
        assert_eq!(described, "system configuration `models`");
    }
}

/// A session's isolation is gated like anything else, and the three constraints are separate.
#[cfg(test)]
mod sandbox_rules {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nucleos::contract::{Principal, SandboxPermissions, SandboxPolicy, SessionId};

    /// The same request the sibling module's helper builds, kept local so this module is self-contained.
    async fn check<'a>(document: Document, subject: Gated<'a>) -> Verdict {
        let principal = Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        };
        let ruleset = Ruleset::new(document).unwrap();
        let session = SessionId::new("s1");
        ruleset
            .check(PolicyRequest {
                principal: &principal,
                session: Some(&session),
                subject,
            })
            .await
    }

    fn workspace_write(extra: &[&str], network: bool) -> SandboxPolicy {
        SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: extra.iter().map(|root| (*root).to_owned()).collect(),
            network_access: network,
        }
    }

    /// A rule with no `sandbox` grants no session, whatever else it allows.
    #[tokio::test]
    async fn a_policy_without_a_sandbox_rule_opens_nothing() {
        let document =
            Document::parse(r#"{"default":"deny","rules":[{"decision":"allow","tool":"*"}]}"#)
                .unwrap();
        let policy = workspace_write(&[], false);
        let verdict = check(
            document,
            Gated::Sandbox {
                policy: &policy,
                workspace: Some("/srv/ws"),
            },
        )
        .await;
        assert!(matches!(verdict, Verdict::Deny { .. }), "{verdict:?}");
    }

    /// The kind is matched by glob, so a deployment can allow one rung and not the next.
    #[tokio::test]
    async fn the_kind_is_what_the_rule_names() {
        let document = Document::parse(
            r#"{"default":"deny","rules":[{"decision":"allow","sandbox":"read_only"}]}"#,
        )
        .unwrap();
        assert_eq!(
            check(
                Document::clone(&document),
                Gated::Sandbox {
                    policy: &SandboxPolicy::ReadOnly {
                        network_access: false
                    },
                    workspace: None,
                },
            )
            .await,
            Verdict::Allow
        );
        assert!(
            matches!(
                check(
                    Document::clone(&document),
                    Gated::Sandbox {
                        policy: &SandboxPolicy::DangerFullAccess,
                        workspace: None,
                    },
                )
                .await,
                Verdict::Deny { .. }
            ),
            "one rung allowed is not the ladder allowed"
        );
    }

    /// Network is not part of the kind, so it is denied unless the rule says otherwise.
    #[tokio::test]
    async fn network_is_denied_unless_the_rule_allows_it() {
        let without = Document::parse(
            r#"{"default":"deny","rules":[{"decision":"allow","sandbox":"workspace_write"}]}"#,
        )
        .unwrap();
        let networked = workspace_write(&[], true);
        assert!(matches!(
            check(
                Document::clone(&without),
                Gated::Sandbox {
                    policy: &networked,
                    workspace: None,
                },
            )
            .await,
            Verdict::Deny { .. }
        ));

        let with = Document::parse(
            r#"{"default":"deny","rules":[{"decision":"allow","sandbox":"workspace_write","network":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            check(
                with,
                Gated::Sandbox {
                    policy: &networked,
                    workspace: None,
                },
            )
            .await,
            Verdict::Allow
        );
    }

    /// Asking for more than the session's policy is a widening, and needs a rule that allows it.
    ///
    /// `with_additional_permissions` is additive, and the gate used to look only at
    /// `require_escalated` — so a plain `program: "sh"` rule also granted whatever roots and network
    /// the caller appended.
    #[tokio::test]
    async fn additional_permissions_need_an_escalation_rule() {
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "make".to_owned()];
        let policy = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let additional = nucleos::contract::AdditionalPermissions {
            writable_roots: vec!["/".to_owned()],
            network_access: true,
        };
        let permissions = SandboxPermissions::WithAdditionalPermissions { additional };

        let plain =
            Document::parse(r#"{"default":"deny","rules":[{"decision":"allow","program":"sh"}]}"#)
                .unwrap();
        assert!(matches!(
            check(
                Document::clone(&plain),
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &permissions,
                },
            )
            .await,
            Verdict::Deny { .. }
        ));

        let widened = Document::parse(
            r#"{"default":"deny","rules":[{"decision":"allow","program":"sh","escalation":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            check(
                widened,
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &permissions,
                },
            )
            .await,
            Verdict::Allow
        );

        // Running under the session's own policy is not a widening, and needs no such rule.
        assert_eq!(
            check(
                plain,
                Gated::Exec {
                    argv: &argv,
                    policy: &policy,
                    permissions: &SandboxPermissions::UseDefault,
                },
            )
            .await,
            Verdict::Allow
        );
    }

    /// A writable root outside the prefix is refused, and the extra roots count as roots.
    ///
    /// The workspace is a request field, so "may write in its workspace" can only mean something if a
    /// rule says which directories those are.
    #[tokio::test]
    async fn a_writable_root_outside_the_prefix_is_refused() {
        let document = Document::parse(
            r#"{"default":"deny","rules":[
                 {"decision":"allow","sandbox":"workspace_write","workspace_prefix":"/srv/"}
               ]}"#,
        )
        .unwrap();

        assert_eq!(
            check(
                Document::clone(&document),
                Gated::Sandbox {
                    policy: &workspace_write(&["/srv/cache"], false),
                    workspace: Some("/srv/ws"),
                },
            )
            .await,
            Verdict::Allow
        );

        // The workspace is fine, the extra root is not: bounding one and not the other would leave
        // the widening reachable through the field nobody bounded.
        assert!(matches!(
            check(
                Document::clone(&document),
                Gated::Sandbox {
                    policy: &workspace_write(&["/etc"], false),
                    workspace: Some("/srv/ws"),
                },
            )
            .await,
            Verdict::Deny { .. }
        ));

        assert!(matches!(
            check(
                document,
                Gated::Sandbox {
                    policy: &workspace_write(&[], false),
                    workspace: Some("/"),
                },
            )
            .await,
            Verdict::Deny { .. }
        ));
    }
}
