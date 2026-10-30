//! A real policy: a document, checked deny-first, that fails closed.
//!
//! The kernel asks one question — may this run? — and this crate answers it from a file the
//! deployment ships. Four properties are the whole design:
//!
//! - **Deny by default.** A rule that matches nothing is the normal case; the default decides what
//!   happens then, and it is `deny` unless a document says otherwise in so many words.
//! - **Deny is evaluated before allow, and before ask.** The order is by decision, not by file
//!   order, so moving a line cannot widen access. A deployment cannot reorder its way into an
//!   outage or into an escalation.
//! - **A rule must name something.** A rule with no `tool`, `model` or `program` matches not at
//!   all — a rule that matches everything by omission is how a policy silently becomes an
//!   allow-list of one entry.
//! - **The program is matched as a fact, not as a sentence.** `argv` is not a shell: the kernel
//!   spawns it directly, so `program` is compared against the basename of `argv[0]` and
//!   `argv_prefix` against the following arguments. No command-line parsing, so no parser to
//!   disagree with the one that runs the command.
//!
//! What is deliberately absent: an expression language, rule priorities, time windows, quotas.
//! Each of those turns a document a human can read into a program, and the kernel already has a
//! place for programs — the host that implements [`Policy`].

use async_trait::async_trait;
use nucleos::adapter::{Gated, Policy, PolicyRequest, Verdict};
use nucleos::contract::{ErrorCode, PermissionKind, PermissionOption};
use nucleos::error::{Error, Result};
use std::path::Path;

/// What a rule, or the document's default, decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Run it.
    Allow,
    /// Refuse it.
    Deny,
    /// Park the session and ask a human.
    Ask,
}

/// What happens when no rule matches.
///
/// Named for the question rather than for the syntax: "default" describes a JSON field, "unmatched"
/// describes the decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmatched {
    /// Refuse anything no rule allows. The default, and the only one that is not a decision a
    /// deployment forgot to make.
    #[default]
    Deny,
    /// Run anything no rule refuses.
    Allow,
}

/// One rule.
///
/// Every field is optional, and a rule that sets none of the matchers never matches: an empty rule
/// is a mistake, and a mistake must not become permission.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Rule {
    /// What it decides.
    pub decision: Option<Decision>,
    /// A glob on a tool name. `*` matches any run of characters, including none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A glob on a model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The program of a command, compared exactly against the basename of `argv[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Leading arguments that must match exactly, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_prefix: Vec<String>,
    /// Only this tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Only this subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// What to tell the caller, or the model, when this rule decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What to ask a human, when `decision` is `ask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
}

/// A policy document, as shipped by a deployment.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Document {
    /// What happens when no rule matches.
    #[serde(default, rename = "default")]
    pub unmatched: Unmatched,
    /// The rules. Order does not decide anything; the decision does.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Document {
    /// Read a document from a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|error| {
            Error::invalid(format!("policy {} is unreadable: {error}", path.display()))
        })?;
        let document: Self = serde_json::from_str(&text).map_err(|error| {
            Error::invalid(format!("policy {} is not valid: {error}", path.display()))
        })?;
        document.validate()?;
        Ok(document)
    }

    /// Refuse a document that could not mean what it says.
    pub fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            let Some(decision) = rule.decision else {
                return Err(Error::invalid(
                    "every rule needs a decision: allow, deny or ask",
                ));
            };
            if rule.tool.is_none() && rule.model.is_none() && rule.program.is_none() {
                return Err(Error::invalid(format!(
                    "a rule with no tool, model or program matches nothing; {decision:?} on \
                     everything is written as a rule with `tool: \"*\"`"
                )));
            }
            if !rule.argv_prefix.is_empty() && rule.program.is_none() {
                return Err(Error::invalid(
                    "an argv_prefix without a program names no command; argv belongs to one",
                ));
            }
            if decision == Decision::Ask && rule.question.is_none() {
                return Err(Error::invalid(
                    "an ask rule needs a question: a human cannot be asked nothing",
                ));
            }
        }
        Ok(())
    }
}

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
                reason: format!(
                    "no policy rule allows {}",
                    describe(&request.subject)
                ),
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
            question: rule.question.clone().unwrap_or_else(|| {
                format!("{} needs approval", describe(&request.subject))
            }),
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
        Gated::Tool { call } => rule
            .tool
            .as_ref()
            .is_some_and(|pattern| glob(pattern, &call.name)),
        Gated::Model { model, .. } => {
            rule.model.as_ref().is_some_and(|pattern| glob(pattern, model))
        }
        // A rule that names a program applies to commands only. `argv_prefix` narrows within it.
        Gated::Exec { argv, .. } => {
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
            check(document.clone(), Gated::Exec {
                argv: &allowed,
                policy: &policy,
                permissions: &permissions,
            })
            .await,
            Verdict::Allow,
            "the basename is what a rule names"
        );

        // A different program that happens to be spelled the same way as an argument is not it.
        let other = argv(&["ls; rm", "-la"]);
        assert!(matches!(
            check(document, Gated::Exec {
                argv: &other,
                policy: &policy,
                permissions: &permissions,
            })
            .await,
            Verdict::Deny { .. }
        ));
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

        let verdict = check(document, Gated::Exec {
            argv: &argv,
            policy: &policy,
            permissions: &permissions,
        })
        .await;
        let Verdict::AskHuman { question, options } = verdict else {
            panic!("expected a question, got {verdict:?}");
        };
        assert_eq!(question, "Push to the remote?");
        // No "always allow": remembering is the host's decision, not a rule's promise.
        assert!(options
            .iter()
            .all(|option| option.kind != PermissionKind::AllowAlways));
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
            check(document.clone(), Gated::Model {
                model: "gpt-4o-mini",
                purpose: Purpose::Answer,
            })
            .await,
            Verdict::Allow
        );
        assert!(matches!(
            check(document, Gated::Model {
                model: "claude",
                purpose: Purpose::Answer,
            })
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
        assert!(document.rules.iter().any(|rule| rule.decision == Some(Decision::Allow)));
        assert!(document.rules.iter().any(|rule| rule.decision == Some(Decision::Deny)));
    }

    #[test]
    fn a_glob_is_anchored_and_star_only() {
        assert!(glob("read_*", "read_file"));
        assert!(glob("*", "anything"));
        assert!(glob("*file", "read_file"));
        assert!(glob("read*file", "read_any_file"));
        assert!(glob("exact", "exact"));

        assert!(!glob("read_*", "write_file"));
        assert!(!glob("read_*", "xread_file"), "an unanchored match would pass");
        assert!(!glob("exact", "exactly"), "no implicit prefix matching");
        assert!(!glob("read_?", "read_a"), "? is not a wildcard here");
    }
}
