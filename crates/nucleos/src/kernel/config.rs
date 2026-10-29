//! Changing a session while it runs — and starting it in the state you would have changed it to.
//!
//! A session's *wiring* — kind, sandbox, workspace — is decided when it opens and is not the
//! application's to change. Everything else is content or strategy: the standing instructions, how
//! answers are shaped, which route is taken, which tools are exposed. Those change during a real
//! conversation, and forcing a new session to change them would throw away the context and the
//! ledger that made the conversation worth having.
//!
//! Two invariants make this safe:
//!
//! - **Every change is an event.** A replay has to know which system prompt was in force at turn
//!   42, or it rebuilds a conversation that never happened.
//! - **The tool allow-list only narrows.** Widening it would let an application route around the
//!   generation it is pinned to, which is what `docs/pitfalls.md` §7 exists to prevent.

use super::{Kernel, session};
use crate::contract::{ConfigPatch, EventKind};
use crate::error::{Error, Result};

impl Kernel {
    /// Apply a partial update, record it, and say which fields changed.
    ///
    /// Absent fields are unchanged; a doubly-optional field set to `null` is cleared.
    pub async fn configure(
        &self,
        session: &mut session::Session,
        patch: ConfigPatch,
    ) -> Result<Vec<&'static str>> {
        if session.is_closed() {
            return Err(Error::invalid(format!(
                "session {} is closed and cannot be configured",
                session.id
            )));
        }

        let changed = self.apply_patch(session, &patch)?;
        if changed.is_empty() {
            // An empty patch is not an event: a log full of "nothing happened" is a log nobody
            // reads.
            return Ok(changed);
        }

        self.record(session, EventKind::SessionConfigured { patch })
            .await?;
        self.checkpoint(session).await?;
        Ok(changed)
    }

    /// Validate and apply, without recording anything.
    ///
    /// Shared with `open`, so a session may start in exactly the states it could later be changed
    /// into. Two validators would eventually disagree about what is legal — and the one that runs
    /// at open is the one nobody tests twice.
    pub(crate) fn apply_patch(
        &self,
        session: &mut session::Session,
        patch: &ConfigPatch,
    ) -> Result<Vec<&'static str>> {
        let mut changed = Vec::new();

        if let Some(system) = &patch.system {
            session.system = system.clone();
            changed.push("system");
        }
        if let Some(params) = &patch.params {
            // Replaced, not merged: every field of `GenerateParams` is already optional, so a merge
            // would make "unset the ceiling" impossible to express.
            session.params = params.clone();
            changed.push("params");
        }
        if let Some(hints) = &patch.hints {
            session.hints = session.hints.merged(hints);
            changed.push("hints");
        }
        if let Some(routing) = &patch.routing {
            // Validated before it is stored: a route naming a model that does not exist is a
            // configuration error, and discovering it on the next turn is discovering it too late.
            self.resolve_route(routing)?;
            session.routing = routing.clone();
            session.retries = routing.retries;
            changed.push("routing");
        }
        if let Some(tools) = &patch.tools {
            narrow_only(&session.tools, tools)?;
            session.tools = tools.clone();
            changed.push("tools");
            // Pruning against the previous policy would be pruning against the wrong one.
            self.trim_pool(session);
        }
        if let Some(policy) = &patch.tool_policy {
            // Checked against the ceiling that is now in force, so a patch that both narrows the
            // ceiling and names an exposure is judged the way it will actually run.
            self.validate_policy(session, policy)?;
            session.tool_policy = policy.clone();
            self.seed_search_policy(session);
            changed.push("tool_policy");
        }

        Ok(changed)
    }
}

impl Kernel {
    /// Make the search tool visible unless the caller said otherwise.
    ///
    /// There is one tool the model must never lose sight of: the one that finds the others. A
    /// policy that does not mention it gets it as `Always`, so the default discovers; a policy that
    /// hides it is refused by [`ToolPolicy::validate`], because a catalog with no door is worse than
    /// a catalog that is too loud.
    pub(crate) fn seed_search_policy(&self, session: &mut session::Session) {
        if self.discoverable() {
            session
                .tool_policy
                .exposures
                .entry(super::tool_search::NAME.to_owned())
                .or_insert(crate::contract::Exposure::Always);
        }
    }

    /// Refuse a policy that names a tool outside the ceiling, or that hides discovery's own tool.
    ///
    /// Exposure is not permission, but it is still bounded by permission: showing the model a tool
    /// it may never execute would produce a call the gate refuses, for a reason the model cannot
    /// see from where it sits.
    fn validate_policy(
        &self,
        session: &session::Session,
        policy: &crate::contract::ToolPolicy,
    ) -> Result<()> {
        if self.discoverable() {
            policy.validate(super::tool_search::NAME)?;
        }
        let ceiling = if session.tools.is_empty() {
            self.tools.keys().cloned().collect()
        } else {
            session.tools.clone()
        };
        // Only an `always` entry can appear in a prompt unconditionally, so it is the only level
        // that has to be inside the ceiling. Deferred entries are filtered by the pool anyway.
        if let Some(outside) = policy
            .exposures
            .iter()
            .filter(|(_, exposure)| **exposure == crate::contract::Exposure::Always)
            .map(|(name, _)| name)
            .find(|name| !ceiling.contains(name))
        {
            return Err(Error::denied(format!(
                "always exposure names {outside:?}, which is outside the session's tool ceiling"
            )));
        }
        if policy.budget.max_definitions == 0 {
            return Err(Error::invalid("max_definitions must be at least 1"));
        }
        Ok(())
    }
}

/// Refuse to widen the tool allow-list.
///
/// An empty list means "every registered tool", so going from a list to empty is the widest change
/// there is, and going from empty to a list is a narrowing.
fn narrow_only(current: &[String], next: &[String]) -> Result<()> {
    if next.is_empty() && !current.is_empty() {
        return Err(Error::denied(
            "the tool allow-list may only narrow: an empty list exposes every registered tool",
        ));
    }
    if current.is_empty() {
        // There is no list to widen from: anything is a narrowing.
        return Ok(());
    }
    if let Some(added) = next.iter().find(|tool| !current.contains(tool)) {
        return Err(Error::denied(format!(
            "the tool allow-list may only narrow: {added:?} was not in it"
        )));
    }
    Ok(())
}
