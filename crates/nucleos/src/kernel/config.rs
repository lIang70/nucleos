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
        }

        Ok(changed)
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
