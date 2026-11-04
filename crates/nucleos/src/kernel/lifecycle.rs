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
        // Scoped to the tenant, so a second tenant opening "s1" gets its own row rather than the
        // first tenant's history. Within one tenant an id still belongs to whoever created it.
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };
        if let Some(existing) = self.store.snapshot(&key).await?
            && existing.principal.subject != principal.subject
        {
            return Err(Error::denied(format!(
                "session {id} belongs to another subject"
            )));
        }

        let now = self.now();
        let (generation, tools) = {
            let generations = self.generations();
            (generations.current(), generations.current_set())
        };
        let head = self.log_head(
            &crate::adapter::SessionKey {
                tenant: principal.tenant.clone(),
                id: id.clone(),
            },
            0,
        );
        let mut session = session::Session::new(session::New {
            id,
            principal: principal.clone(),
            generation,
            tools,
            log: head,
            budget: spec.budget.clone(),
            credit_tokens: spec.credit_tokens,
            now_unix_millis: now,
        });
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
            // A session with no route takes the deployment's first model. Without this, a model added
            // by request would be usable only by a session that named it, and every caller that
            // relied on the default would keep failing until the daemon restarted — which is the
            // change-not-surviving-a-restart problem, in the other direction.
            if session.routing.chain.is_empty()
                && let Some(first) = self.models().first()
            {
                session.routing.chain = vec![first.clone()];
            }
            // Resolved and filtered: a route naming a model this tenant may not use fails here, before
            // a session exists, with a message about the tenant rather than about the model.
            self.permit_route(&session.principal, self.resolve_route(&session.routing)?)?;
        }
        self.emit_state(&mut session).await?;
        self.write_checkpoint(&mut session).await?;
        Ok(session)
    }
    /// Load a session from its checkpoint.
    ///
    /// The spec is not re-sent: the checkpoint carries the route, the parameters and the tool set,
    /// because a resume that had to be told its own configuration again is a resume that can
    /// silently behave differently.
    pub async fn load(&self, principal: &Principal, id: &SessionId) -> Result<session::Session> {
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };
        let snapshot = self
            .store
            .snapshot(&key)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        self.restore(principal, id, snapshot, true).await
    }

    /// Load a session for a reader that needs its state, not its conversation.
    ///
    /// The same restore, over a checkpoint whose `messages` the store never parsed. What comes back is
    /// a session with an empty history and an exact everything-else: the tail is folded identically, so
    /// a view cannot disagree with the log. Use `load` when the conversation is the point.
    pub async fn load_head(
        &self,
        principal: &Principal,
        id: &SessionId,
    ) -> Result<session::Session> {
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };
        let snapshot = self
            .store
            .snapshot_head(&key)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checkpoint for session {id}")))?;
        self.restore(principal, id, snapshot, false).await
    }

    /// Bring one checkpoint up to the log's head.
    ///
    /// `conversation` says whether the caller wants the message history. A head folds the same tail
    /// and then drops the messages it collected: the state it returns is exact, and the conversation
    /// it would have to keep is the thing it exists to avoid.
    async fn restore(
        &self,
        principal: &Principal,
        id: &SessionId,
        snapshot: crate::contract::Snapshot,
        conversation: bool,
    ) -> Result<session::Session> {
        // The lookup itself is scoped to the tenant, so another tenant's session is not "forbidden",
        // it does not exist: the query cannot reach it.
        let key = crate::adapter::SessionKey {
            tenant: principal.tenant.clone(),
            id: id.clone(),
        };

        // Within a tenant, an id is still its creator's. `NotFound` rather than `Denied`: a refusal
        // that says "this exists but is not yours" enumerates the identifiers other subjects use.
        if snapshot.principal.subject != principal.subject {
            return Err(Error::not_found(format!("no session {id}")));
        }
        // The set is resolved once, here: a snapshot naming a generation this process never published
        // (a restore into a fresh daemon) falls back to the current one, and that fallback is what the
        // session is now pinned to.
        let (tools, generation) = {
            let generations = self.generations();
            // `set` falls back to the current one for a number this process never published (a restore
            // into a fresh daemon). The session is then pinned to that set, and its number has to say
            // so — a checkpoint recording a generation the session does not hold is a fact that is
            // false the moment it is written.
            let set = generations.set(snapshot.generation);
            let generation = if generations.contains(snapshot.generation) {
                snapshot.generation
            } else {
                generations.current()
            };
            (set, generation)
        };
        // The head of the log, not a copy of its counter. A supervisor may still be writing to this
        // session, in which case its counter is current and this handle must join it rather than
        // re-derive a number from the checkpoint.
        let head = self.log_head(&key, snapshot.seq.saturating_add(1));
        let mut session = session::Session::from_snapshot(
            snapshot.clone(),
            principal.clone(),
            tools,
            head,
            snapshot.budget.clone(),
            self.now(),
        );
        // The number, not just the set: `from_snapshot` copies it from the checkpoint, and the
        // fallback above decided it should be the current one.
        session.generation = generation;

        // The checkpoint is a cache; the log is the truth. Everything written after it — the tokens
        // a turn spent, the messages it completed, a configuration change — is folded in here, or a
        // reload hands back a session that owes money it forgets and forgets work it was told.
        let tail = self.store.events_since(&key, snapshot.seq + 1).await?;
        let mut unfinished = false;
        let mut warned_about_the_future = false;
        for event in &tail {
            // The version is read, not just written. It cannot make an event readable that is not, but
            // it can say why a session is behaving oddly after a downgrade — and once per load is
            // enough, because the alternative is one line per event on a long log.
            if event.schema_version > crate::contract::Event::SCHEMA_VERSION
                && !warned_about_the_future
            {
                warned_about_the_future = true;
                tracing::warn!(
                    event = "event_from_a_newer_build",
                    session = %session.id,
                    written = event.schema_version,
                    understood = crate::contract::Event::SCHEMA_VERSION,
                    consequence = "events this build does not recognise are carried, not applied",
                );
            }
            if super::replay::fold(&mut session, event) == super::replay::Applied::AbandonedTurn {
                unfinished = true;
            }
            // A completed unit ends any evidence before it: a turn that finished is not abandoned,
            // whatever deltas preceded it. This is also what makes a second recovery a no-op — the
            // record written the first time is itself such an event.
            if super::replay::completes_a_unit(&event.kind) {
                unfinished = false;
            }
            // `fetch_max`, not `store`: another writer (a supervisor, a concurrent request) may have
            // moved the head past this event while the tail was being read, and rewinding it would
            // hand out a number that is already in the log.
            session
                .log
                .seq
                .fetch_max(event.seq + 1, std::sync::atomic::Ordering::SeqCst);
            // Folded *and* materialised: the state now contains this event, so a checkpoint taken
            // from here must say so. Claiming less would write a snapshot whose messages carry events
            // its `seq` denies, and the next `load` would fold them a second time.
            session.materialised_through = Some(
                session
                    .materialised_through
                    .map_or(event.seq, |through| through.max(event.seq)),
            );
        }

        // A turn the runtime did not survive is recorded rather than inferred. Nothing else in the
        // log says it happened: the deltas are there, but they look the same as a turn that is still
        // streaming.
        // A head is a read, and a read does not write: recording the abandoned turn belongs to whoever
        // is about to drive the session, which is the caller that asked for `load`.
        if conversation && unfinished && session.state == crate::contract::SessionState::Open {
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

        if !conversation {
            // Both of the things a head leaves out, dropped after folding: the state it returns is
            // exact, and the history it would have to keep is the thing it exists to avoid.
            session.messages.clear();
            session.ledger = crate::contract::Ledger::default();
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
        // The flag first, on its own, because it is what `DELETE` and retention read and a checkpoint
        // can be refused as stale. Then the checkpoint, which is the cache: if it loses the race the
        // session is still closed as far as every reader of the listing table is concerned.
        self.store.mark_closed(&session.key(), self.now()).await?;
        self.write_checkpoint(session).await?;
        // The head is only useful while something can still write: a closed session takes no calls,
        // and a process supervisor holds its own `Arc`. Dropping the kernel's entry lets a long-lived
        // daemon forget sessions that ended, instead of keeping one counter per session forever.
        self.forget_log(&session.key());
        Ok(())
    }

    // -- syscalls -------------------------------------------------------------
}
