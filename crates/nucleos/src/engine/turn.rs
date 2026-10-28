//! The turn loop, and the tool calls inside it.
//!
//! The loop is deliberately small: build a request, walk the route, fold what came back, run the
//! tools, and test four stop conditions. Everything that looks like cleverness belongs in an
//! adapter.

use crate::adapter::{FinishReason, Gated, PolicyRequest, ToolCtx, ToolOutput, Verdict};
use crate::contract::{
    Content, EventKind, Message, Part, PartKind, PendingCall, PermissionKind, PermissionOption,
    Role, Routing, SessionId, SessionState, StopReason, ToolCall, ToolResult, ToolResultPart,
};
use crate::engine::Kernel;
use crate::engine::session::Session;
use crate::engine::{Outcome, SessionSpec};
use crate::error::{Error, Result};
use futures_util::future::join_all;

/// Whether the turn finished its tools or parked on a human.
enum Progress {
    Done,
    Parked,
}

impl Kernel {
    /// Drive a session until it stops, parks, or fails.
    pub(crate) async fn drive(
        &self,
        mut session: Session,
        spec: SessionSpec,
        routing: Routing,
        targets: crate::engine::ResolvedRoute,
    ) -> Result<Outcome> {
        session.retries = routing.retries;
        session.params = spec.params.clone();
        session.system = spec.system.clone();
        session.tools = spec.tools.clone();

        if session.state == SessionState::Created {
            session.transition(SessionState::Running)?;
            self.emit_state(&mut session).await?;
            self.checkpoint(&session).await?;
        }

        loop {
            if let Some(reason) = session.exhausted(self.now()) {
                self.emit(&mut session, EventKind::BudgetExhausted { reason })
                    .await?;
                return self
                    .terminate(&mut session, SessionState::Failed, reason)
                    .await;
            }

            if outstanding_calls(&session).is_empty() {
                // A model turn.
                session.transition(SessionState::Running)?;
                let call = match self.call_model(&mut session, &targets).await {
                    Ok(call) => call,
                    Err(error) => return self.abort(&mut session, error).await,
                };
                session.messages.push(call.message.clone());
                session.steps = session.steps.saturating_add(1);
                let step_index = u32::try_from(session.steps).unwrap_or(u32::MAX);
                self.emit(
                    &mut session,
                    EventKind::MessageCompleted {
                        message: call.message.clone(),
                        step_index,
                    },
                )
                .await?;
                self.checkpoint(&session).await?;

                if outstanding_calls(&session).is_empty() {
                    // Nothing to call. The provider's own reason decides between "answered"
                    // and the two ways an answer can be cut short.
                    return match call.finish {
                        FinishReason::Length => {
                            self.terminate(&mut session, SessionState::Failed, StopReason::Tokens)
                                .await
                        }
                        FinishReason::Filtered => {
                            self.terminate(&mut session, SessionState::Failed, StopReason::Failed)
                                .await
                        }
                        _ => {
                            self.terminate(&mut session, SessionState::Done, StopReason::Completed)
                                .await
                        }
                    };
                }
                continue;
            }

            // A tool turn: the assistant message is in history, its calls are not yet answered.
            session.transition(SessionState::AwaitingTool)?;
            self.emit_state(&mut session).await?;

            match self.run_tools(&mut session).await? {
                Progress::Parked => {
                    self.checkpoint(&session).await?;
                    return Ok(Outcome {
                        session: session.id.clone(),
                        state: session.state,
                        stop: None,
                        ledger: session.ledger.clone(),
                        next_seq: session.seq,
                    });
                }
                Progress::Done => {
                    session.transition(SessionState::Running)?;
                    self.emit_state(&mut session).await?;
                    self.checkpoint(&session).await?;
                }
            }
        }
    }

    /// Run every outstanding call of the current turn.
    ///
    /// Policy is asked about all of them *before* anything runs, so a batch that needs a human
    /// parks with nothing executed. Then the allowed ones run concurrently: state transitions
    /// are serial, tool execution is parallel, and those are different axes.
    async fn run_tools(&self, session: &mut Session) -> Result<Progress> {
        let calls = outstanding_calls(session);
        if calls.is_empty() {
            return Ok(Progress::Done);
        }

        let deadline = self.deadline_for(session);
        let mut verdicts = Vec::with_capacity(calls.len());
        for call in &calls {
            let verdict = self
                .policy()
                .check(PolicyRequest {
                    principal: &session.principal,
                    session: Some(&session.id),
                    subject: Gated::Tool { call },
                })
                .await;
            if let Verdict::AskHuman { question, options } = verdict {
                self.park(session, question, options, call).await?;
                return Ok(Progress::Parked);
            }
            verdicts.push(verdict);
        }

        let session_id = session.id.clone();
        let futures = calls
            .iter()
            .zip(verdicts.into_iter())
            .map(|(call, verdict)| {
                let tool = self.tool(&call.name);
                let args = call.arguments_json.clone();
                let call_id = call.id.clone();
                let ctx_session = session_id.clone();
                let started = self.now();
                async move {
                    let ctx = ToolCtx {
                        session: &ctx_session,
                        call_id: &call_id,
                        deadline_unix_millis: deadline,
                    };
                    let output = match verdict {
                        Verdict::Allow => match tool {
                            // A model that produced broken JSON gets told so, and the turn
                            // continues. Executing it would mean guessing at the arguments.
                            Some(tool) => match serde_json::from_str::<serde_json::Value>(&args) {
                                Ok(_) => tool.invoke(&args, &ctx).await,
                                Err(error) => ToolOutput::failed(format!(
                                    "arguments were not valid JSON: {error}"
                                )),
                            },
                            // A model naming a tool it was not given is a model mistake, and the
                            // model is the one that can recover from it.
                            None => ToolOutput::failed(format!("no tool named {:?}", ctx.call_id)),
                        },
                        Verdict::Deny { reason, .. } => ToolOutput::failed(reason),
                        Verdict::AskHuman { .. } => {
                            ToolOutput::failed("policy asked a human after the batch was released")
                        }
                    };
                    (output, started)
                }
            });
        let outputs = join_all(futures).await;

        for (call, (output, started)) in calls.iter().zip(outputs) {
            let duration = self.now().saturating_sub(started);
            self.finish_call(session, call, output, duration).await?;
        }
        Ok(Progress::Done)
    }

    /// Execute one call outside a batch, for the resume-after-verdict path.
    pub(crate) async fn invoke_tool(
        &self,
        call: &ToolCall,
        session: &SessionId,
        deadline: u64,
    ) -> ToolOutput {
        let ctx = ToolCtx {
            session,
            call_id: &call.id,
            deadline_unix_millis: deadline,
        };
        match self.tool(&call.name) {
            Some(tool) => tool.invoke(&call.arguments_json, &ctx).await,
            None => ToolOutput::failed(format!("no tool named {:?}", call.name)),
        }
    }

    /// Record a call's result: the event, and the message that enters history.
    pub(crate) async fn finish_call(
        &self,
        session: &mut Session,
        call: &ToolCall,
        output: ToolOutput,
        duration_millis: u64,
    ) -> Result<()> {
        let result = ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        };
        self.emit(
            session,
            EventKind::ToolFinished {
                duration_millis,
                result: result.clone(),
            },
        )
        .await?;
        session.messages.push(Message {
            role: Role::Tool,
            content: Content::new([Part::new(PartKind::ToolResult(ToolResultPart { result }))]),
        });
        Ok(())
    }

    /// Park the session on a human, recording the call and the options offered.
    async fn park(
        &self,
        session: &mut Session,
        question: String,
        options: Vec<PermissionOption>,
        call: &ToolCall,
    ) -> Result<()> {
        session.pending = Some(PendingCall {
            call: call.clone(),
            options: options.clone(),
            question: question.clone(),
        });
        session.transition(SessionState::AwaitingHuman)?;
        self.emit(
            session,
            EventKind::AwaitingHuman {
                question,
                tool_call_id: call.id.clone(),
                options,
                // Silence is never approval. A default that allowed would be a security bug with
                // a clock attached.
                default_if_unanswered: PermissionKind::Reject,
            },
        )
        .await?;
        self.emit_state(session).await
    }

    /// Apply the four stop conditions at a boundary.
    async fn terminate(
        &self,
        session: &mut Session,
        state: SessionState,
        reason: StopReason,
    ) -> Result<Outcome> {
        session.transition(state)?;
        self.emit(
            session,
            EventKind::StateChanged {
                state,
                stop: Some(reason),
            },
        )
        .await?;
        self.checkpoint(session).await?;
        Ok(Outcome {
            session: session.id.clone(),
            state: session.state,
            stop: Some(reason),
            ledger: session.ledger.clone(),
            next_seq: session.seq,
        })
    }

    /// Turn a model or policy failure into a recorded outcome rather than an API error.
    ///
    /// The event log is the record. A caller that only gets an `Err` has no history of what
    /// happened before it.
    async fn abort(&self, session: &mut Session, error: Error) -> Result<Outcome> {
        self.emit(
            session,
            EventKind::Failed {
                code: error.code(),
                reason: error.reason().to_owned(),
            },
        )
        .await?;
        self.terminate(session, SessionState::Failed, StopReason::Failed)
            .await
    }

    fn deadline_for(&self, session: &Session) -> u64 {
        match session.budget.max_wall_millis {
            Some(max) => session.started_unix_millis.saturating_add(max),
            None => u64::MAX,
        }
    }
}

/// The calls of the last assistant message that have no result yet.
///
/// Derived from history rather than tracked in a field, so a resumed turn recomputes it — which
/// is what makes answering a parked call idempotent.
fn outstanding_calls(session: &Session) -> Vec<ToolCall> {
    let Some(message) = session.messages.last() else {
        return Vec::new();
    };
    if message.role != Role::Assistant {
        return Vec::new();
    }
    let answered: Vec<&str> = session
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.content.parts.iter())
        .filter_map(|part| match &part.kind {
            PartKind::ToolResult(result) => Some(result.result.call_id.as_str()),
            _ => None,
        })
        .collect();
    message
        .content
        .parts
        .iter()
        .filter_map(|part| match &part.kind {
            PartKind::ToolCall(call) => Some(call.call.clone()),
            _ => None,
        })
        .filter(|call| !answered.contains(&call.id.as_str()))
        .collect()
}
