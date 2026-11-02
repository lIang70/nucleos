//! One model call, as a syscall.
//!
//! The route walk itself lives in `route.rs`; this is the entry point that owns the session's
//! bookkeeping around it.

use super::{InferenceOutcome, Kernel, session};
use crate::contract::{EventKind, Message};
use crate::error::{Error, Result};

impl Kernel {
    /// One inference: walk the route, emit the deltas, record every attempt.
    ///
    /// The messages come from the caller. What you send is what you get, and whether the answer
    /// enters history is decided by whoever asked — which is what makes a verification turn
    /// possible without polluting the transcript.
    pub async fn inference(
        &self,
        session: &mut session::Session,
        messages: &[Message],
    ) -> Result<InferenceOutcome> {
        if session.kind != crate::contract::SessionKind::Agent {
            return Err(Error::invalid(
                "a process session has no model calls: it runs one command",
            ));
        }
        // Before anything is spent, and before the round counter moves: a turn that will not run
        // should leave no trace of having started.
        self.spend(session).await?;
        // A turn boundary is the only moment a session may change tool sets: a prompt built from
        // one generation cannot be answered by another.
        super::generation::migrate(self, session).await?;
        // A turn is the unit the discovery pool expires against, so the counter moves before the
        // prompt is built: what a session shows this turn is decided with this turn's clock.
        self.begin_round(session);
        let routing = session.routing.clone();
        let targets = self.permit_route(&session.principal, self.resolve_route(&routing)?)?;
        // The commit point. Everything above could fail without the log knowing a turn existed, so
        // those failures belong to the caller as a status code. Everything below is written down, and
        // a follower that saw this event is entitled to learn the outcome from the log rather than
        // from a connection that may close.
        self.emit(
            session,
            EventKind::TurnStarted {
                model: targets
                    .first()
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default(),
                generation: session.generation.0,
            },
        )
        .await?;

        // What the log held when the turn was announced. Anything written after this point explains
        // the outcome; nothing written after it means the turn failed without a trace.
        let announced_at = session.next_seq();

        // The turn is registered so a cancel can find it, and the call races the signal. Dropping the
        // call's future is what stops it: the adapter's stream owns the HTTP response, and dropping it
        // closes the connection rather than leaving a request running that nobody reads.
        let cancel = self.begin_turn(&session.id);
        let outcome = tokio::select! {
            result = self.call_model(session, messages, &targets) => result,
            () = cancel.cancelled() => Err(Error::new(
                crate::contract::ErrorCode::Cancelled,
                "the turn was cancelled by the caller",
            )),
        };
        self.end_turn(&session.id);

        let call = match outcome {
            Ok(call) => call,
            Err(error) if error.code() == crate::contract::ErrorCode::Cancelled => {
                // Recorded as a failed attempt, not as a bug: the session stays open, the spend that
                // was already reported stays debited, and the deltas are in the log where a caller can
                // read what the model had said before it was stopped. It is *not* retried — a cancelled
                // turn is a decision, and repeating it would undo the decision.
                self.emit(
                    session,
                    EventKind::AttemptFailed {
                        attempt: crate::contract::Attempt {
                            index: u32::try_from(session.steps).unwrap_or(u32::MAX),
                            model: routing.chain.first().cloned().unwrap_or_default(),
                            phase: crate::contract::AttemptPhase::Execute,
                            trigger: crate::contract::AttemptTrigger::Selection,
                            outcome: crate::contract::AttemptOutcome::Failed {
                                code: crate::contract::ErrorCode::Cancelled,
                                reason: "cancelled by the caller while running".to_owned(),
                            },
                            usage: crate::contract::Usage::default(),
                            duration_millis: 0,
                            degraded: Vec::new(),
                        },
                    },
                )
                .await?;
                self.write_checkpoint(session).await?;
                return Err(error);
            }
            Err(error) => {
                // Nothing about this turn is in the log beyond the announcement. Written here rather
                // than wherever the failure happened, because the places that can fail are many and
                // the rule is one: a turn that produced no record must not look like a turn that is
                // still running. A stream watching it would otherwise simply stop.
                if session.next_seq() == announced_at {
                    self.emit(
                        session,
                        EventKind::TurnFailed {
                            code: error.code(),
                            reason: error.reason().to_owned(),
                        },
                    )
                    .await?;
                }
                return Err(error);
            }
        };
        session.steps = session.steps.saturating_add(1);
        let step_index = u32::try_from(session.steps).unwrap_or(u32::MAX);
        self.emit(
            session,
            EventKind::MessageCompleted {
                message: call.message.clone(),
                step_index,
            },
        )
        .await?;
        self.write_checkpoint(session).await?;
        Ok(InferenceOutcome {
            message: call.message,
            finish: call.finish,
            usage: session.usage.clone(),
        })
    }
}
