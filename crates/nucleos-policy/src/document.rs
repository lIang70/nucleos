//! The file a deployment ships: rules, and the defaults behind them.
//!
//! Nothing here decides anything about a call. It describes what a policy *is* and refuses a document
//! that cannot mean what it says — a rule with no matcher, an `ask` with no question.

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
    /// A glob on a configuration subject: `models`, `sources`, `policy`, `tokens`, `admin_keys`.
    ///
    /// Configuration rules are separate from tool rules on purpose. A deployment that lets a caller
    /// run `read_file` has said nothing at all about whether it may add a model — which is the
    /// ability to point every session's traffic somewhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// Which configuration scope: `system` or `tenant`.
    ///
    /// Narrows a `config` rule; meaningless without one. A tenant may hold a rule for `tenant` and
    /// still never reach the system scope, because the request carries the scope it was made
    /// against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The program of a command, compared exactly against the basename of `argv[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Leading arguments that must match exactly, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_prefix: Vec<String>,
    /// Whether this rule also covers a command that asks to run **outside** its sandbox.
    ///
    /// A separate field because it is a separate decision. `program: "git"` answers "may git run"; it
    /// does not answer "may git run with the sandbox removed", and letting one answer both is how a
    /// caller lifts its own confinement by setting a flag in the request. Default `false`: an
    /// escalation needs a rule that says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<bool>,
    /// A glob over the sandbox a session asks to be created with: `read_only`, `workspace_write`,
    /// `danger_full_access`, `external_sandbox`.
    ///
    /// Its own field for the same reason `escalation` is: "may this caller open a session" and "with
    /// how much isolation" are different questions, and letting one answer both is how a caller picks
    /// its own confinement. A rule without `sandbox` grants no session at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Whether a session under this rule may reach the network.
    ///
    /// Absent means no. Network access is not part of the sandbox *kind*, so a rule that allowed
    /// `workspace_write` would otherwise also allow it with the network on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Every writable root a session asks for must start with this.
    ///
    /// `workspace_write` grants writes under the session's workspace, and the workspace is a request
    /// field — so without a prefix, "may write in its workspace" means "may write wherever it says its
    /// workspace is". Both the workspace and any extra roots a caller adds are checked against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_prefix: Option<String>,
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

    /// Read a document from text rather than from a file.
    ///
    /// The daemon keeps its policy in the system store, so the parser has to be reachable without a
    /// path — and the validation must be the same one a file gets, or two entry points would drift.
    pub fn parse(text: &str) -> Result<Self> {
        let document: Self = serde_json::from_str(text)
            .map_err(|error| Error::invalid(format!("policy is not valid: {error}")))?;
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
            // The specific mistake first: a rule carrying only `scope` is one edit away from being
            // right, and "matches nothing" would send its author looking in the wrong place.
            if rule.scope.is_some() && rule.config.is_none() {
                return Err(Error::invalid(
                    "a `scope` only narrows a `config` rule; on its own it names nothing",
                ));
            }
            if rule.tool.is_none()
                && rule.model.is_none()
                && rule.program.is_none()
                && rule.config.is_none()
                && rule.sandbox.is_none()
            {
                return Err(Error::invalid(format!(
                    "a rule with no tool, model, program, config or sandbox matches nothing; \
                     {decision:?} on everything is written as a rule with `tool: \"*\"`"
                )));
            }
            // The narrowing fields only narrow what they belong to.
            if (rule.network.is_some() || rule.workspace_prefix.is_some()) && rule.sandbox.is_none()
            {
                return Err(Error::invalid(
                    "`network` and `workspace_prefix` only narrow a `sandbox` rule; on their own they \
                     name nothing",
                ));
            }
            // `system`, `tenant`, `own`, or `tenant/<id>` for one named tenant. A bare tenant name is
            // not accepted: the document cannot tell one from a typo, and "matches nothing" is the
            // failure mode that sends its author looking in the wrong place.
            if let Some(scope) = &rule.scope
                && scope != "system"
                && scope != "tenant"
                && scope != "own"
                && !scope
                    .strip_prefix("tenant/")
                    .is_some_and(|tenant| !tenant.is_empty())
            {
                return Err(Error::invalid(format!(
                    "config scope {scope:?} is not `system`, `tenant`, `own` or `tenant/<id>`"
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
