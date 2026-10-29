//! Whether a call may happen.

use crate::contract::{
    ErrorCode, PermissionOption, Principal, Purpose, SandboxPermissions, SandboxPolicy, SessionId,
    ToolCall,
};
use async_trait::async_trait;

/// The kernel's only policy question.
///
/// Asynchronous because a real policy evaluates against something — a document, a remote
/// service, a database — and a synchronous signature would push implementors toward blocking
/// a runtime worker. The cost is a boxed future on every gated call, which is cheaper than
/// the class of bug it prevents.
#[async_trait]
pub trait Policy: Send + Sync {
    /// Decide, before the tool runs.
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict;
}

/// What is being gated.
///
/// One trait, two questions. A model call is a spend, and a runtime that gates tools but not
/// models has a policy with a hole where the money is.
#[derive(Debug, Clone, Copy)]
pub enum Gated<'a> {
    /// A tool is about to run.
    Tool {
        /// The call.
        call: &'a ToolCall,
    },
    /// A model is about to be called.
    Model {
        /// Which model.
        model: &'a str,
        /// Why.
        purpose: Purpose,
    },
    /// A command is about to run.
    ///
    /// The three fields are what a policy needs to decide: you cannot judge "run this" without
    /// seeing what it may touch, and you cannot judge an escalation without seeing what it wants.
    Exec {
        /// The command line, verbatim.
        argv: &'a [String],
        /// The session's policy, before any widening.
        policy: &'a SandboxPolicy,
        /// What this command asks for on top.
        permissions: &'a SandboxPermissions,
    },
}

/// What policy is given.
///
/// No session state, no token counts: a policy that can see the budget will eventually start
/// making budget decisions, and that is the kernel's job.
#[derive(Debug)]
pub struct PolicyRequest<'a> {
    /// Who is asking.
    pub principal: &'a Principal,
    /// Which session, when there is one. `Generate` has none.
    pub session: Option<&'a SessionId>,
    /// The thing being gated.
    pub subject: Gated<'a>,
}

/// What policy decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Refuse it. The session continues; the model learns why.
    Deny {
        /// Why, in a code the caller can act on.
        code: ErrorCode,
        /// Why, for humans.
        reason: String,
    },
    /// Ask a human, and park the session until they answer.
    AskHuman {
        /// What to ask.
        question: String,
        /// What may be answered. The kernel reports the choice; it never decides that
        /// "always allow" applies — that is the host's policy.
        options: Vec<PermissionOption>,
    },
}

/// A policy that allows everything.
///
/// The name is a warning: a deployment that never replaces this has no policy, and its
/// deny-by-default claim is a sentence in a document.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl Policy for AllowAll {
    async fn check(&self, _request: PolicyRequest<'_>) -> Verdict {
        Verdict::Allow
    }
}

/// A policy that refuses everything, for testing the refusal path.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl Policy for DenyAll {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        let what = match request.subject {
            Gated::Tool { call } => call.name.clone(),
            Gated::Model { model, .. } => model.to_owned(),
            Gated::Exec { argv, .. } => argv.join(" "),
        };
        Verdict::Deny {
            code: crate::contract::ErrorCode::Denied,
            reason: format!("{what} is not available"),
        }
    }
}

