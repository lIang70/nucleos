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
        let targets = self.resolve_route(&routing)?;
        let call = self.call_model(session, messages, &targets).await?;
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
