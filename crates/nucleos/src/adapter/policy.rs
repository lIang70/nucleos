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
    /// Configuration is about to change.
    ///
    /// Configuration is a privilege before it is a convenience: whoever may add a model may point a
    /// deployment's traffic at their own server, and whoever may replace the policy may replace it
    /// with one that says yes. So a change is gated like a tool call, and — unlike a tool call — a
    /// gate that answers "ask a human" refuses instead of parking: there is no session to park.
    Config {
        /// Whether this is the deployment's configuration or one tenant's.
        scope: ConfigScope<'a>,
        /// What is being changed: `models`, `sources`, `policy`, `tokens`, `admin_keys`.
        ///
        /// A name rather than a path, so a policy rule reads `{"config": "models"}` and not a URL.
        subject: &'a str,
    },
    /// A session is about to be created, with the isolation it asked for.
    ///
    /// A separate question from [`Gated::Exec`], asked once at `open`: the sandbox is a property of the
    /// whole session, decided there and carried in the checkpoint, so it cannot be judged per command.
    /// Without this, "the caller names its own policy" means any tenant can name `danger_full_access`,
    /// and [`crate::contract::SandboxPolicy`]'s own documentation says a request may not do that.
    Sandbox {
        /// What the caller asked for.
        policy: &'a crate::contract::SandboxPolicy,
        /// Where its commands would run, which bounds the writable roots a policy grants.
        workspace: Option<&'a str>,
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

/// Which configuration a change touches.
///
/// Two scopes because they are two different authorities: the deployment's configuration is the
/// operator's, and a tenant's narrowing is the tenant's. A tenant may not edit the system scope even
/// if the policy would allow the same subject, which is why the scope travels with the request
/// instead of being implied by the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope<'a> {
    /// The whole deployment: models, sources, tokens, the system policy.
    System,
    /// One tenant's narrowing of the above.
    Tenant(&'a str),
}

impl std::fmt::Display for ConfigScope<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::Tenant(tenant) => write!(f, "tenant/{tenant}"),
        }
    }
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
            Gated::Config { scope, subject } => format!("{scope} configuration: {subject}"),
            Gated::Sandbox { policy, .. } => format!("a `{}` session", policy.kind()),
        };
        Verdict::Deny {
            code: crate::contract::ErrorCode::Denied,
            reason: format!("{what} is not available"),
        }
    }
}
