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
