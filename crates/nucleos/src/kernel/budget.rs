//! Budget is credit, and the kernel is the one that checks it.
//!
//! Every other invariant in this kernel is enforced where the value is owned. Budget was the
//! exception: `Session::exhausted` existed and the reference loop was its only caller, so a driver
//! that forgot to ask could spend forever — and the event that recorded the stop was written by the
//! driver, which meant the log recorded a claim rather than a decision.
//!
//! The check now sits in front of every syscall that spends. It is a gate, not a report: the call
//! does not happen, the session closes, and the reason it closed is the kernel's own.

use super::{Kernel, session};
use crate::contract::{EventKind, StopReason};
use crate::error::{Error, Result};

impl Kernel {
    /// Grant more credit to a session, and revive one that stopped on a budget.
    ///
    /// Two acts in one call, because they are the same act from the caller's side: topping up a running
    /// session, and bringing back one that ran out of money. A session that stopped for any other reason
    /// — completed, cancelled, failed, denied — is *done*, not out of credit, and is refused.
    ///
    /// `amount` is `Some(tokens)` to grant that many, or `None` to remove the limit. The balance after
    /// the grant is recorded in the log, because a replay has to be able to tell a revived session from
    /// one that never stopped.
    pub async fn credit(&self, session: &mut session::Session, amount: Option<u64>) -> Result<()> {
        if amount == Some(0) {
            return Err(Error::invalid(
                "a grant must be for some tokens, or None for no limit",
            ));
        }
        if session.is_closed() {
            // Only a budget stop may be revived. Everything else is an ending, and a caller that wants
            // to undo one of those wants a new session, not more money.
            let budget_stop = session.stop_reason().is_some_and(|reason| {
                matches!(
                    reason,
                    StopReason::Tokens | StopReason::Cost | StopReason::Steps | StopReason::Wall
                )
            });
            if !budget_stop {
                return Err(Error::invalid(format!(
                    "session {} is closed ({:?}), and only a session that stopped on a budget can be \
                     credited",
                    session.id,
                    session.stop_reason()
                )));
            }
        }
        let remaining = match (session.credit_tokens, amount) {
            // Removing the limit is a decision, not an arithmetic overflow.
            (_, None) => None,
            // A session with no limit keeps it: there is nothing to top up.
            (None, Some(_)) => None,
            (Some(current), Some(grant)) => Some(current.saturating_add(grant)),
        };

        // A grant moves the balance; it cannot move a ceiling. `max_steps`, `max_tokens`, `max_cost`
        // and `max_wall` were fixed at `Start` and stay fixed, so a session that stopped on one of
        // those would be "revived" into a state it cannot run in — reporting a stop reason of `None`
        // and then refusing the very next call. Checked before anything is written, so a refusal
        // leaves the session exactly as it was.
        let previous = (session.credit_tokens, session.state, session.stop);
        session.credit_tokens = remaining;
        if session.is_closed()
            && let Some(reason) = session.exhausted(self.now())
        {
            session.credit_tokens = previous.0;
            return Err(Error::new(
                crate::contract::ErrorCode::Budget,
                format!(
                    "session {} stopped on a budget a grant cannot move ({reason:?}): its ceilings \
                     were fixed when it started, and more credit does not raise them",
                    session.id
                ),
            ));
        }
        if session.is_closed() {
            session.state = crate::contract::SessionState::Open;
            session.stop = None;
        }
        self.emit(
            session,
            crate::contract::EventKind::Credited { amount, remaining },
        )
        .await?;
        // Forced, not throttled: the grant is what the next turn spends against, and a crash between
        // the answer and the checkpoint would otherwise lose it.
        self.write_checkpoint(session).await
    }

    /// Refuse a call that has no room to run.
    ///
    /// Called before a model call and before a batch of tools, which are the two ways this kernel
    /// spends. Idempotent: a closed session is refused by [`Kernel::spend`] without writing a second
    /// `budget_exhausted`, because two identical events for one stop is a log a replay has to guess
    /// about.
    pub(crate) async fn spend(&self, session: &mut session::Session) -> Result<()> {
        // Closed first: a session that already stopped must not write a second stop. Without this
        // order the exhaustion check keeps firing — the credit is still zero — and one stop becomes
        // one event per attempt, which a replay would have to interpret.
        if session.is_closed() {
            return Err(Error::invalid(format!(
                "session {} is closed: {}",
                session.id,
                session
                    .stop_reason()
                    .map_or("no reason recorded".to_owned(), |reason| format!(
                        "{reason:?}"
                    ))
            )));
        }
        if let Some(reason) = session.exhausted(self.now()) {
            tracing::warn!(
                event = "budget_refused",
                session = %session.id,
                stop = ?reason,
                steps = session.steps,
                tokens = session.usage.total_tokens(),
                credit = session.credit_tokens,
            );
            self.stop_for_budget(session, reason).await?;
            return Err(Error::new(
                crate::contract::ErrorCode::Budget,
                match reason {
                    StopReason::Steps => "the session has used every step it was allowed",
                    StopReason::Tokens => "the session has spent every token it was granted",
                    StopReason::Cost => "the session has spent every unit of its cost budget",
                    StopReason::Wall => "the session has been open longer than its budget allows",
                    _ => "the session is out of budget",
                },
            ));
        }
        Ok(())
    }

    /// Record the stop and close, once.
    async fn stop_for_budget(
        &self,
        session: &mut session::Session,
        reason: StopReason,
    ) -> Result<()> {
        // The event says the budget ran out; the state change says what happened next. Both, in
        // this order, so a reader never sees a closed session with no explanation.
        self.emit(session, EventKind::BudgetExhausted { reason })
            .await?;
        self.close(session, reason).await
    }
}
