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
        // An identifier belongs to whoever created it. Without this, a second tenant opening "s1"
        // would take over the first tenant's history rather than starting its own.
        if let Some(existing) = self.store.snapshot(&id).await?
            && existing.principal != *principal
        {
            return Err(Error::denied(format!(
                "session {id} belongs to another tenant"
            )));
        }

        let now = self.now();
        let mut session = session::Session::new(
            id,
            principal.clone(),
            self.generation(),
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
        self.seed_search_policy(&mut session);
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

        // The store keys sessions by id alone, so this is the check that makes an id private to its
        // tenant. `NotFound` rather than `Denied`: a refusal that says "this exists but is not yours"
        // tells a caller which identifiers other tenants are using.
        if snapshot.principal != *principal {
            return Err(Error::not_found(format!("no session {id}")));
        }
        let mut session = session::Session::from_snapshot(
            snapshot.clone(),
            principal.clone(),
            Default::default(),
            self.now(),
        );

        // The checkpoint is a cache; the log is the truth. Everything written after it — the tokens
        // a turn spent, the messages it completed, a configuration change — is folded in here, or a
        // reload hands back a session that owes money it forgets and forgets work it was told.
        let tail = self.store.events_since(id, snapshot.seq + 1).await?;
        let mut unfinished = false;
        for event in &tail {
            if super::replay::fold(&mut session, event) == super::replay::Applied::AbandonedTurn {
                unfinished = true;
            }
            // A completed unit ends any evidence before it: a turn that finished is not abandoned,
            // whatever deltas preceded it. This is also what makes a second recovery a no-op — the
            // record written the first time is itself such an event.
            if super::replay::completes_a_unit(&event.kind) {
                unfinished = false;
            }
            session
                .seq
                .store(event.seq + 1, std::sync::atomic::Ordering::SeqCst);
        }

        // A turn the runtime did not survive is recorded rather than inferred. Nothing else in the
        // log says it happened: the deltas are there, but they look the same as a turn that is still
        // streaming.
        if unfinished && session.state == crate::contract::SessionState::Open {
            let attempt = crate::contract::Attempt {
                index: u32::try_from(session.ledger.len()).unwrap_or(u32::MAX),
                model: session.routing.chain.first().cloned().unwrap_or_default(),
                phase: crate::contract::AttemptPhase::Execute,
                trigger: crate::contract::AttemptTrigger::Selection,
                outcome: crate::contract::AttemptOutcome::Failed {
                    code: crate::contract::ErrorCode::Unavailable,
                    reason: super::replay::ABANDONED.to_owned(),
                },
                usage: crate::contract::Usage::default(),
                duration_millis: 0,
                degraded: Vec::new(),
            };
            self.emit(
                &mut session,
                crate::contract::EventKind::AttemptFailed { attempt },
            )
            .await?;
        }
        Ok(session)
    }
    /// Close a session. Further calls are refused.
    pub async fn close(&self, session: &mut session::Session, reason: StopReason) -> Result<()> {
        if !session.close_with(reason)? {
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
