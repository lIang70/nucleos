//! Whether a call may happen.

use crate::contract::{ErrorCode, PermissionOption, Principal, SessionId, ToolCall};
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

/// What policy is given.
///
/// No session state, no token counts, no model name: a policy that can see the budget will
/// eventually start making budget decisions, and that is the kernel's job.
#[derive(Debug)]
pub struct PolicyRequest<'a> {
    /// Who is asking.
    pub principal: &'a Principal,
    /// Which session.
    pub session: &'a SessionId,
    /// The call being gated.
    pub call: &'a ToolCall,
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
