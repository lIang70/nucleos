//! The route walk: pick a target, retry it, fall back, and write down what each attempt cost.
//!
//! One rule shapes the whole file: **an attempt that produced content is never retried.** A
//! retry after a streamed fragment would put two partial answers in one turn's history, and no
//! replay could tell which one the model was supposed to be continuing.

use crate::adapter::{
    FinishReason, Gated, Model, ModelDelta, ModelRequest, PolicyRequest, Verdict,
};
use crate::contract::{
    Attempt, AttemptOutcome, AttemptPhase, AttemptTrigger, Content, Degraded, EventKind, Message,
    Part, PartKind, Purpose, ReasoningPart, TextPart, ToolCall, ToolCallPart, Usage,
};
use crate::error::{Error, Result};
use crate::kernel::Kernel;
use crate::kernel::session::Session;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What a successful model call produced.
pub(crate) struct CallResult {
    pub message: Message,
    pub finish: FinishReason,
}

/// How one attempt ended, before it becomes a ledger entry.
enum Ending {
    Answered {
        message: Message,
        finish: FinishReason,
        usage: Usage,
        degraded: Vec<Degraded>,
    },
    Failed {
        error: Error,
        usage: Usage,
        produced: bool,
        degraded: Vec<Degraded>,
        /// Where it failed: before the call (`Preflight`) or during it (`Execute`).
        phase: AttemptPhase,
    },
}

/// The parts of an answer as they arrive.
#[derive(Default)]
struct Assembling {
    text: BTreeMap<u32, String>,
    reasoning: BTreeMap<u32, ReasoningAccum>,
    calls: BTreeMap<u32, CallAccum>,
    usage: Usage,
    degraded: Vec<Degraded>,
    produced: bool,
}

#[derive(Default)]
struct ReasoningAccum {
    text: String,
    signature: String,
}

#[derive(Default)]
struct CallAccum {
    id: String,
    name: String,
    arguments: String,
}

impl Assembling {
    /// Turn the fragments into the message that enters history.
    ///
    /// Order is fixed — reasoning, then text, then calls — so that two runs of the same deltas
    /// produce the same message. A history whose part order depends on arrival order cannot be
    /// replayed.
    fn finish(self) -> Message {
        let mut parts = Vec::new();
        for accum in self.reasoning.into_values() {
            parts.push(Part::new(PartKind::Reasoning(ReasoningPart {
                text: accum.text,
                signature: accum.signature,
                id: String::new(),
                source: String::new(),
            })));
        }
        for text in self.text.into_values() {
            parts.push(Part::new(PartKind::Text(TextPart { text })));
        }
        for accum in self.calls.into_values() {
            parts.push(Part::new(PartKind::ToolCall(ToolCallPart {
                call: ToolCall {
                    id: accum.id,
                    name: accum.name,
                    arguments_json: accum.arguments,
                    meta: BTreeMap::new(),
                },
            })));
        }
        Message {
            role: crate::contract::Role::Assistant,
            content: Content::new(parts),
        }
    }
}

impl Kernel {
    /// Walk the route until something answers.
    pub(crate) async fn call_model(
        &self,
        session: &mut Session,
        messages: &[Message],
        targets: &[(String, Arc<dyn Model>)],
    ) -> Result<CallResult> {
        let retries = session.retries;
        let mut attempt_index = 0u32;
        let mut last_error: Option<Error> = None;

        for (target_index, (name, model)) in targets.iter().enumerate() {
            // Gate the spend before spending it. A runtime that gates tools but not model
            // calls has a policy with a hole exactly where the money is.
            let verdict = self
                .policy()
                .check(PolicyRequest {
                    principal: &session.principal,
                    session: Some(&session.id),
                    subject: Gated::Model {
                        model: name,
                        purpose: Purpose::Answer,
                    },
                })
                .await;
            match verdict {
                Verdict::Allow => {}
                Verdict::Deny { code, reason } => {
                    let mut error = Error::new(code, reason);
                    error = error.detail("model", name.clone());
                    return Err(error);
                }
                Verdict::AskHuman { .. } => {
                    return Err(Error::internal(
                        "policy asked a human about a model call; AskHuman is only meaningful for tools",
                    ));
                }
            }

            for retry in 0..=retries {
                let trigger = match (target_index, retry) {
                    (0, 0) => AttemptTrigger::Selection,
                    (_, 0) => AttemptTrigger::Fallback,
                    _ => AttemptTrigger::Retry,
                };
                let started = self.now();

                match self
                    .invoke_once(session, messages, model, name, attempt_index)
                    .await?
                {
                    Ending::Answered {
                        message,
                        finish,
                        usage,
                        degraded,
                    } => {
                        session.ledger.record(Attempt {
                            index: attempt_index,
                            model: name.clone(),
                            phase: AttemptPhase::Execute,
                            trigger,
                            outcome: AttemptOutcome::Ok,
                            usage,
                            duration_millis: self.now().saturating_sub(started),
                            degraded,
                        });
                        return Ok(CallResult { message, finish });
                    }
                    Ending::Failed {
                        error,
                        usage,
                        produced,
                        degraded,
                        phase,
                    } => {
                        let attempt = Attempt {
                            index: attempt_index,
                            model: name.clone(),
                            phase,
                            trigger,
                            outcome: AttemptOutcome::Failed {
                                code: error.code(),
                                reason: error.reason().to_owned(),
                            },
                            usage,
                            duration_millis: self.now().saturating_sub(started),
                            degraded,
                        };
                        // A failed attempt that streamed still paid, so it is recorded and
                        // reported rather than retried into a second partial answer.
                        self.emit(
                            session,
                            EventKind::AttemptFailed {
                                attempt: attempt.clone(),
                            },
                        )
                        .await?;
                        session.ledger.record(attempt);
                        attempt_index = attempt_index.saturating_add(1);

                        if produced {
                            return Err(error);
                        }
                        tracing::warn!(
                            event = "attempt_failed",
                            model = %name,
                            trigger = ?trigger,
                            code = ?error.code(),
                        );
                        last_error = Some(error);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::unavailable("no model could be reached")))
    }

    /// Refuse a prompt the model cannot take.
    ///
    /// `prompt + output <= window`, with the completion reserve taken from the request when it says
    /// so and from the model's declaration otherwise.
    fn measure(&self, model: &Arc<dyn Model>, request: &ModelRequest) -> Result<()> {
        let capability = model.capability();
        let window = u64::from(capability.context_window);
        if window == 0 {
            // A model that declares no window is one nothing here can hold to a limit. Treating that
            // as "unlimited" would be the silent truncation this check exists to prevent, so it is
            // a refusal to run rather than a guess.
            return Err(Error::invalid(
                "the model declares no context window, so a prompt cannot be shown to fit",
            ));
        }
        let prompt = model.count_tokens(request)?;
        let reserve = u64::from(
            request
                .params
                .max_output_tokens
                .unwrap_or(capability.max_output_tokens),
        );
        if prompt.saturating_add(reserve) > window {
            tracing::warn!(
                event = "prompt_does_not_fit",
                prompt_tokens = prompt,
                reserve = reserve,
                window = window,
            );
            return Err(Error::invalid(format!(
                "the prompt is {prompt} tokens and the completion reserve is {reserve}, over a \
                 window of {window}; the caller has to compact or raise the budget"
            )));
        }
        Ok(())
    }

    /// One call, streaming its deltas into events as they arrive.
    async fn invoke_once(
        &self,
        session: &mut Session,
        messages: &[Message],
        model: &Arc<dyn Model>,
        name: &str,
        attempt_index: u32,
    ) -> Result<Ending> {
        let request = self.build_request(session, messages);

        // What must fit, before anything is spent: a provider that silently truncates changes the
        // answer without saying so, so the refusal happens here, where the window and the prompt are
        // both in hand. This is the only thing the kernel may decide about context — *what* to drop
        // is the caller's, because the caller is the one who assembled this list.
        if let Err(error) = self.measure(model, &request) {
            return Ok(Ending::Failed {
                error,
                usage: Usage::default(),
                produced: false,
                degraded: Vec::new(),
                phase: AttemptPhase::Preflight,
            });
        }

        let mut stream = match model.invoke(request).await {
            Ok(stream) => stream,
            Err(error) => {
                return Ok(Ending::Failed {
                    error,
                    usage: Usage::default(),
                    produced: false,
                    degraded: Vec::new(),
                    phase: AttemptPhase::Execute,
                });
            }
        };

        use tokio_stream::StreamExt;
        let mut acc = Assembling::default();
        let mut finish = None;

        loop {
            // Waiting for the next fragment is also the moment a buffered one becomes late: a model
            // that pauses for a second must not hold the last token of a burst in memory for a second,
            // so the wait itself is timed. A continuous stream never reaches the timer — the next
            // fragment arrives first, and the batch keeps growing.
            let delta = if session.buffered_events.is_empty() {
                stream.next().await
            } else {
                tokio::select! {
                    delta = stream.next() => delta,
                    () = tokio::time::sleep(super::state::FLUSH_AFTER) => {
                        self.flush(session).await?;
                        continue;
                    }
                }
            };
            let Some(delta) = delta else {
                break;
            };
            match delta {
                Ok(ModelDelta::Text { part_index, text }) => {
                    acc.produced = true;
                    acc.text.entry(part_index).or_default().push_str(&text);
                    self.emit(session, EventKind::TextDelta { part_index, text })
                        .await?;
                }
                Ok(ModelDelta::Reasoning {
                    part_index,
                    text,
                    signature,
                }) => {
                    acc.produced = true;
                    let entry = acc.reasoning.entry(part_index).or_default();
                    entry.text.push_str(&text);
                    entry.signature.push_str(&signature);
                    self.emit(
                        session,
                        EventKind::ReasoningDelta {
                            part_index,
                            text,
                            signature,
                        },
                    )
                    .await?;
                }
                Ok(ModelDelta::ToolCallStarted { index, id, name }) => {
                    acc.produced = true;
                    let entry = acc.calls.entry(index).or_default();
                    entry.id = id.clone();
                    entry.name = name.clone();
                    self.emit(
                        session,
                        EventKind::ToolCallStarted {
                            index,
                            tool_call_id: id,
                            name,
                            deadline_unix_millis: self.deadline(session),
                            progress_token: String::new(),
                        },
                    )
                    .await?;
                }
                Ok(ModelDelta::ToolCallArgs { index, fragment }) => {
                    acc.produced = true;
                    acc.calls
                        .entry(index)
                        .or_default()
                        .arguments
                        .push_str(&fragment);
                    self.emit(session, EventKind::ToolCallArgs { index, fragment })
                        .await?;
                }
                Ok(ModelDelta::Usage(usage)) => {
                    acc.usage.add(&usage);
                    self.debit(session, &usage);
                    self.emit(
                        session,
                        EventKind::Usage {
                            usage,
                            purpose: Purpose::Answer,
                            model: name.to_owned(),
                            attempt_index,
                        },
                    )
                    .await?;
                }
                Ok(ModelDelta::Dropped { field, reason }) => {
                    let degraded = Degraded {
                        field: field.to_owned(),
                        reason,
                    };
                    acc.degraded.push(degraded.clone());
                    self.emit(session, EventKind::CapabilityDegraded(degraded))
                        .await?;
                }
                Ok(ModelDelta::Finished { reason }) => {
                    finish = Some(reason);
                }
                Err(error) => {
                    // Whatever the stream produced before it broke is already the answer a reader has
                    // seen; it goes to the store before the failure is recorded.
                    self.flush(session).await?;
                    return Ok(Ending::Failed {
                        error,
                        usage: acc.usage,
                        produced: acc.produced,
                        degraded: acc.degraded,
                        phase: AttemptPhase::Execute,
                    });
                }
            }
        }
        // The stream ended: the last fragments are not waiting for an event that will never come.
        self.flush(session).await?;

        let usage = std::mem::take(&mut acc.usage);
        let degraded = std::mem::take(&mut acc.degraded);
        Ok(Ending::Answered {
            message: acc.finish(),
            finish: finish.unwrap_or(FinishReason::Stop),
            usage,
            degraded,
        })
    }

    /// What this turn is asking for.
    ///
    /// The messages come from the caller, not from the session: an inference is a pure call —
    /// what you send is what you get, and whether the answer enters history is the caller's
    /// decision. That is what makes a verification turn possible without polluting the transcript.
    fn build_request(&self, session: &Session, messages: &[Message]) -> ModelRequest {
        ModelRequest {
            system: session.system.clone(),
            messages: messages.into(),
            tools: self.tool_specs(session),
            params: session.params.clone(),
            hints: session.hints.clone(),
            purpose: Purpose::Answer,
        }
    }

    /// Spend credit, and record the spend whether or not the answer turns out to be usable.
    fn debit(&self, session: &mut Session, usage: &Usage) {
        session.usage.add(usage);
        session.turn_usage.add(usage);
        if let Some(credit) = session.credit_tokens {
            session.credit_tokens = Some(credit.saturating_sub(usage.total_tokens()));
        }
    }
}
