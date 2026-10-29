//! Opening, loading and closing a session.
//!
//! The three doors into a session's life, and the only places one is created or ends.

use super::{Kernel, SessionSpec, session};
use crate::contract::{EventKind, Principal, SessionId, SessionState, StopReason};
use crate::error::{Error, Result};

impl Kernel {
    /// Open a session.
    pub async fn open(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
    ) -> Result<session::Session> {
        let now = self.now();
        let mut session = session::Session::new(
            id,
            principal.clone(),
            self.generation,
            spec.budget.clone(),
            spec.credit_tokens,
            now,
        );
        // The kernel's default route, then whatever the caller asked to start from.
        session.routing = self.routing.clone();
        session.retries = session.routing.retries.max(self.retries);
        session.workspace = spec.workspace.clone();
        session.sandbox = spec.sandbox.clone();
        session.kind = spec.kind;
        session.process = match (spec.kind, spec.process.clone()) {
            (crate::contract::SessionKind::Process, Some(process)) => {
                Some(crate::contract::ProcessState {
                    spec: process,
                    status: crate::contract::ProcessStatus::Pending,
                })
            }
            // A process session with nothing to run is a request that cannot mean anything.
            (crate::contract::SessionKind::Process, None) => {
                return Err(Error::invalid(
                    "a process session needs a process spec to start",
                ));
            }
            (crate::contract::SessionKind::Agent, _) => None,
        };
        session.messages = spec.initial.clone();
        // The same path `configure` uses, so a session can start in any state it could later be
        // changed into. A process session has no route — validating one it will never use would
        // make it refuse to start over a model.
        self.apply_patch(&mut session, &spec.config)?;
        if session.kind == crate::contract::SessionKind::Agent {
            self.models.resolve(&session.routing)?;
        }
        self.emit_state(&mut session).await?;
        self.write_checkpoint(&session).await?;
        Ok(session)
    }
    /// Load a session from its checkpoint.
    ///
    /// The spec is not re-sent: the checkpoint carries the route, the parameters and the tool set,
    /// because a resume that had to be told its own configuration again is a resume that can
    /// silently behave differently.
    pub async fn load(&self, principal: &Principal, id: &SessionId) -> Result<session::Session> {
        let snapshot = self
            .store
            .snapshot(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        Ok(session::Session::from_snapshot(
            snapshot,
            principal.clone(),
            Default::default(),
            self.now(),
        ))
    }
    /// Close a session. Further calls are refused.
    pub async fn close(&self, session: &mut session::Session, reason: StopReason) -> Result<()> {
        if !session.close()? {
            return Ok(());
        }
        self.emit(
            session,
            EventKind::StateChanged {
                state: SessionState::Closed,
                stop: Some(reason),
            },
        )
        .await?;
        self.write_checkpoint(session).await
    }

    // -- syscalls -------------------------------------------------------------
}
