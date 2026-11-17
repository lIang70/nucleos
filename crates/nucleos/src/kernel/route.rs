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
    /// The provider's response identifier, when it reported one.
    pub response_id: Option<String>,
    /// The provider's request identifier, when it reported one.
    pub request_id: Option<String>,
}

/// How one attempt ended, before it becomes a ledger entry.
enum Ending {
    Answered {
        message: Message,
        finish: FinishReason,
        usage: Usage,
        degraded: Vec<Degraded>,
        /// The provider's response identifier, when it reported one.
        response_id: Option<String>,
        /// The provider's request identifier, when it reported one.
        request_id: Option<String>,
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
    /// Content parts by index, so a modality needs no new field here either. Text and reasoning are
    /// fragments that merge; anything else arrives whole and replaces what is at its index.
    parts: BTreeMap<u32, PartAccum>,
    calls: BTreeMap<u32, CallAccum>,
    /// Which part index each tool call occupies. Kept beside `calls` because argument fragments are
    /// keyed by the tool-call ordinal, but the message is ordered by part index: two numbers with two
    /// meanings, rather than one number doing both jobs badly.
    call_parts: BTreeMap<u32, u32>,
    /// The first report defines the accumulator, later ones add to it — the same rule the ledger
    /// follows, and for the same reason: adding a first report to a total that was never a report
    /// turns every reported cost into an unknown one.
    usage: Option<Usage>,
    produced: bool,
    /// The provider's response identifier, when it reported one.
    response_id: Option<String>,
    /// The provider's request identifier, when it reported one.
    request_id: Option<String>,
}

/// One content part under construction.
enum PartAccum {
    /// Text fragments, appended in arrival order.
    Text(String),
    /// Reasoning fragments: text and the provider's signature, both appended.
    Reasoning { text: String, signature: String },
    /// A part that arrived whole.
    Whole(Part),
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
    /// Order is by `part_index`, then tool calls — so two runs of the same deltas produce the same
    /// message. A history whose part order depends on arrival order cannot be replayed.
    fn finish(self) -> Result<Message> {
        // One ordered sequence: content parts and tool calls share the part-index space, so the
        // message order is the order the provider produced them. Appending calls after all content
        // would make the event stream and the stored message describe different turns.
        let mut parts = Vec::with_capacity(self.parts.len() + self.call_parts.len());
        let mut calls = self.calls;
        let mut call_parts = self.call_parts.into_iter().peekable();
        let mut content = self.parts.into_iter().peekable();

        loop {
            let next_call = call_parts.peek().map(|(part_index, _)| *part_index);
            let next_content = content.peek().map(|(part_index, _)| *part_index);
            match (next_call, next_content) {
                (Some(call_index), Some(content_index)) if call_index < content_index => {
                    parts.push(tool_call_part(
                        &mut calls,
                        call_parts.next().map(|(_, index)| index),
                    ));
                }
                (Some(_), Some(_)) | (None, Some(_)) => {
                    // The peek above established that this is `Some`.
                    if let Some((_, accum)) = content.next() {
                        parts.push(finish_part(accum));
                    }
                }
                (Some(_), None) => {
                    parts.push(tool_call_part(
                        &mut calls,
                        call_parts.next().map(|(_, index)| index),
                    ));
                }
                (None, None) => break,
            }
        }

        // Every announced call carries a part index, and no two things may claim one, so nothing can
        // be left over — `DeltaGuard` is what guarantees that. This is the backstop that makes the
        // guarantee load-bearing: before it, a call without a position was silently deleted from the
        // answer, which is the one outcome this kernel does not allow to be quiet.
        if !calls.is_empty() {
            return Err(Error::new(
                crate::contract::ErrorCode::AdapterContractViolation,
                format!(
                    "{} tool call(s) reached assembly without a place in the message: the answer \
                     would lose them",
                    calls.len()
                ),
            ));
        }

        Ok(Message {
            role: crate::contract::Role::Assistant,
            content: Content::new(parts),
        })
    }
}

fn finish_part(accum: PartAccum) -> Part {
    match accum {
        PartAccum::Text(text) => Part::new(PartKind::Text(TextPart { text })),
        PartAccum::Reasoning { text, signature } => Part::new(PartKind::Reasoning(ReasoningPart {
            text,
            signature,
            id: String::new(),
            source: String::new(),
        })),
        PartAccum::Whole(part) => part,
    }
}

fn tool_call_part(calls: &mut BTreeMap<u32, CallAccum>, index: Option<u32>) -> Part {
    let accum = index
        .and_then(|index| calls.remove(&index))
        .unwrap_or_default();
    Part::new(PartKind::ToolCall(ToolCallPart {
        call: ToolCall {
            id: accum.id,
            name: accum.name,
            arguments_json: accum.arguments,
            meta: BTreeMap::new(),
        },
    }))
}

impl Kernel {
    /// Walk the route until something answers.
    pub(crate) async fn call_model(
        &self,
        session: &mut Session,
        messages: &[Message],
        targets: &[(String, Arc<dyn Model>)],
        overrides: &crate::kernel::InferenceOverrides,
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
                    .invoke_once(session, messages, model, name, attempt_index, overrides)
                    .await?
                {
                    Ending::Answered {
                        message,
                        finish,
                        usage,
                        degraded,
                        response_id,
                        request_id,
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
                        return Ok(CallResult {
                            message,
                            finish,
                            response_id,
                            request_id,
                        });
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

    /// Refuse a request that contradicts itself, before any model is consulted.
    ///
    /// These are not about the model's ability, and that is why they come first: a request that
    /// mixes two intentions is wrong whoever it is sent to, and discovering it from a provider's 400
    /// costs a round trip and a line in the log that says nothing a caller can act on.
    fn coherent(request: &ModelRequest) -> Result<()> {
        use crate::contract::ToolChoice;

        // Tools answer in text. An image or audio request carrying a toolset is a caller who
        // configured two things at once, and the provider either refuses the whole call or silently
        // picks one.
        if request.output != crate::contract::Output::Text && !request.tools.is_empty() {
            return Err(Error::invalid(format!(
                "tools apply to text output only, and this request asks for {:?}: drop one of them",
                request.output
            )));
        }
        match &request.params.tool_choice {
            // A forced call needs something to force. The provider would refuse the request, or
            // worse, quietly treat it as `auto`.
            ToolChoice::Required | ToolChoice::Function { .. } if request.tools.is_empty() => {
                Err(Error::invalid(format!(
                    "tool choice {:?} requires at least one tool, and this request carries none",
                    request.params.tool_choice
                )))
            }
            // A name that is not in the list is a call the model cannot make: the tool was not in
            // the prompt, so the provider sees a decision it did not offer.
            ToolChoice::Function { name }
                if !request.tools.iter().any(|tool| &tool.name == name) =>
            {
                Err(Error::invalid(format!(
                    "tool choice names {name:?}, which this request does not offer: the model cannot \
                     call a tool it was not shown"
                )))
            }
            _ => Ok(()),
        }
    }

    /// Refuse a prompt the model cannot take.
    ///
    /// `prompt + output <= window`, with the completion reserve taken from the request when it says
    /// so and from the model's declaration otherwise.
    /// Refuse what this model says it cannot do.
    ///
    /// Every field of [`crate::contract::Capability`] is checked here or nowhere: a declaration the
    /// kernel does not enforce is a guarantee a caller reads and the session does not keep. The
    /// refusals are `UnsupportedFeature` — the request is well formed and this model is not the one
    /// to serve it — and they all happen before `prepare`, so nothing is compiled or sent.
    ///
    /// The line between this and [`Kernel::measure`]: an absence that makes the call pointless is
    /// refused here, and an absence that only lowers quality is the adapter's [`crate::contract::Degraded`]
    /// report. A forced tool choice that becomes "maybe" is the first; a reasoning effort with no
    /// channel is the second.
    fn enforce(model: &Arc<dyn Model>, request: &ModelRequest) -> Result<()> {
        use crate::contract::{ErrorCode, TextFormat, ToolChoice};

        let capability = model.capability();
        let unsupported = |detail: String| Error::new(ErrorCode::UnsupportedFeature, detail);

        // What this model declares about content kinds, checked before anything is counted or sent.
        // An empty list means "undeclared", not "none": an adapter that has not been taught to
        // declare must keep working, and the compiler stays the final arbiter.
        if !capability.produces_undeclared_outputs() {
            let wanted = request.output.part_kind();
            if !capability.outputs.contains(&wanted) {
                return Err(unsupported(format!(
                    "this model produces {:?} but not {wanted:?}: ask for one it declares, or route \
                     to a model that does",
                    capability.outputs
                )));
            }
        }
        if !capability.accepts_undeclared_inputs() {
            for message in request.messages.iter() {
                for part in &message.content.parts {
                    let kind = part.kind.name();
                    if !capability.inputs.contains(&kind) {
                        return Err(unsupported(format!(
                            "this model does not accept {kind:?} input (it declares {:?})",
                            capability.inputs
                        )));
                    }
                }
            }
        }
        if !request.tools.is_empty() && !capability.tool_calling {
            return Err(unsupported(
                "this model does not accept tool definitions, and the request carries tools"
                    .to_owned(),
            ));
        }
        // A forced call is a guarantee, and one the model has said it cannot keep is not a guarantee
        // to send anyway. `required` becomes a suggestion on a provider that ignores it, and a
        // `function` choice becomes a tool call that may never happen — either way an extraction
        // step gets an answer it cannot use, having paid for it.
        let forces = matches!(
            request.params.tool_choice,
            ToolChoice::Required | ToolChoice::Function { .. }
        );
        if forces && !capability.tool_choice_required {
            return Err(unsupported(format!(
                "this model cannot be made to call a tool, and the request forces {:?}: it would be \
                 honoured as a suggestion, or not at all",
                request.params.tool_choice
            )));
        }
        // A schema the model cannot be held to buys a refusal or an unvalidated answer, and neither
        // is the call the caller asked for. `json_object` is the weaker promise and is allowed: a
        // provider that emits an object without honouring the schema still returns an object.
        if request.params.format != TextFormat::Text
            && matches!(request.params.format, TextFormat::JsonSchema { .. })
            && !capability.structured_output
        {
            return Err(unsupported(
                "this model cannot be held to a JSON Schema, and the request asks for one: the \
                 answer would be unvalidated, which is not what the caller requested"
                    .to_owned(),
            ));
        }
        // A token count is not a byte count. A small prompt carrying an inline clip can still be
        // larger than the model accepts, and that is knowable before the upload.
        if let Some(limit) = capability.max_input_bytes {
            for message in request.messages.iter() {
                for part in &message.content.parts {
                    let bytes = match &part.kind {
                        PartKind::Image(part) => part.source.bytes(),
                        PartKind::Audio(part) => part.source.bytes(),
                        PartKind::Video(part) => part.source.bytes(),
                        _ => None,
                    };
                    if let Some(bytes) = bytes {
                        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                        if size > limit {
                            return Err(Error::invalid(format!(
                                "an inline media part is {size} bytes, over this model's declared \
                                 limit of {limit}: send a reference or compact the input"
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Refuse a prompt the model cannot fit.
    ///
    /// `prompt + reserve <= window`, with the completion reserve taken from the request when it says
    /// so and from the model's declaration otherwise. This is the only check here that costs
    /// anything — `count_tokens` is an adapter call — which is why the capability refusals happen
    /// first.
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
        overrides: &crate::kernel::InferenceOverrides,
    ) -> Result<Ending> {
        let request = self.build_request(session, messages, overrides);

        // Does the request contradict itself? Nothing is spent on it either way, so it is settled
        // before the model's own limits are consulted: a caller who asked for an image *and* tools
        // has a request problem, not a capacity one.
        if let Err(error) = Self::coherent(&request) {
            return Ok(Ending::Failed {
                error,
                usage: Usage::default(),
                produced: false,
                degraded: Vec::new(),
                phase: AttemptPhase::Preflight,
            });
        }

        // Can this model do it at all? Every capability it declares is checked here, so a
        // guarantee the caller set cannot be quietly downgraded by being sent to a model that said
        // it cannot keep it. Before `measure`, because that one costs a token count.
        if let Err(error) = Self::enforce(model, &request) {
            return Ok(Ending::Failed {
                error,
                usage: Usage::default(),
                produced: false,
                degraded: Vec::new(),
                phase: AttemptPhase::Preflight,
            });
        }

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

        // What the adapter says it will do with the request, before it does it — and the compiled
        // request it will do it with. One call, because a plan derived from one compilation and a
        // body from another is a promise nobody keeps: the check would pass on the request the
        // adapter described and the money would be spent on the one it sent.
        //
        // The fields are taken before the request moves into `prepare`: they are what the plan is
        // checked against, and they are a property of what the caller asked for, not of what the
        // adapter compiled.
        let active = request.active_fields();
        let prepared = match model.prepare(request) {
            Ok(prepared) => prepared,
            Err(error) => {
                return Ok(Ending::Failed {
                    error,
                    usage: Usage::default(),
                    produced: false,
                    degraded: Vec::new(),
                    phase: AttemptPhase::Preflight,
                });
            }
        };
        // The plan is the whole account of what was not honoured, and it is the same list the
        // artifact was built from. Recording it here rather than collecting it from the stream is
        // what makes the attempt's record and the artifact agree: a degradation discovered while
        // reading deltas could only ever be a second opinion.
        let degraded = prepared.plan().degraded.clone();
        if let Err(error) = contract(model, &active, prepared.plan()) {
            return Ok(Ending::Failed {
                error,
                usage: Usage::default(),
                produced: false,
                degraded,
                phase: AttemptPhase::Preflight,
            });
        }
        // Said now rather than inferred later: a client learns that a knob did not take effect at the
        // start of the turn, not from an answer that looks slightly wrong. Emitted once, from the
        // plan: the stream does not repeat it.
        for entry in &degraded {
            self.emit(session, EventKind::CapabilityDegraded(entry.clone()))
                .await?;
        }

        let mut stream = match model.invoke(prepared).await {
            Ok(stream) => stream,
            Err(error) => {
                return Ok(Ending::Failed {
                    error,
                    usage: Usage::default(),
                    produced: false,
                    degraded,
                    phase: AttemptPhase::Execute,
                });
            }
        };

        use tokio_stream::StreamExt;
        let mut acc = Assembling::default();
        // One place decides whether this stream is shaped correctly, and it is the same one an
        // adapter runs against itself in `testing::conformance`. A stream only the test helper
        // rejects is a stream production accepts.
        let mut guard = crate::adapter::DeltaGuard::new();

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
            // Shape before meaning: a delta that cannot belong to a well-formed stream is refused
            // here, before anything is recorded about it.
            if let Ok(delta) = &delta
                && let Err(error) = guard.accept(delta)
            {
                // A structurally broken stream may have produced content a client already
                // rendered, so it stays; the turn fails rather than assembling something the
                // model did not say.
                self.flush(session).await?;
                return Ok(Ending::Failed {
                    error,
                    usage: acc.usage.unwrap_or_default(),
                    produced: acc.produced,
                    degraded,
                    phase: AttemptPhase::Execute,
                });
            }
            match delta {
                Ok(ModelDelta::PartAppend { part_index, part }) => {
                    acc.produced = true;
                    match &part.kind {
                        PartKind::Text(text) => {
                            match acc
                                .parts
                                .entry(part_index)
                                .or_insert_with(|| PartAccum::Text(String::new()))
                            {
                                PartAccum::Text(accumulated) => accumulated.push_str(&text.text),
                                // A whole part was here and fragments follow: the fragments win,
                                // because they are the more specific answer to "what is at this
                                // index".
                                other => *other = PartAccum::Text(text.text.clone()),
                            }
                        }
                        PartKind::Reasoning(reasoning) => {
                            match acc.parts.entry(part_index).or_insert_with(|| {
                                PartAccum::Reasoning {
                                    text: String::new(),
                                    signature: String::new(),
                                }
                            }) {
                                PartAccum::Reasoning {
                                    text: accumulated,
                                    signature,
                                } => {
                                    accumulated.push_str(&reasoning.text);
                                    signature.push_str(&reasoning.signature);
                                }
                                other => {
                                    *other = PartAccum::Reasoning {
                                        text: reasoning.text.clone(),
                                        signature: reasoning.signature.clone(),
                                    }
                                }
                            }
                        }
                        // A media part that streams: the bytes append to its source, which is the
                        // shape synthesized audio needs — many chunks at one index, one clip.
                        PartKind::Image(_) | PartKind::Audio(_) | PartKind::Video(_) => {
                            let merged = match acc.parts.get_mut(&part_index) {
                                Some(PartAccum::Whole(existing)) => append_media(existing, &part),
                                _ => false,
                            };
                            if !merged {
                                acc.parts.insert(part_index, PartAccum::Whole(part.clone()));
                            }
                        }
                        // Nothing else has append semantics. An adapter that means "this whole
                        // part is the answer" says so with `PartReplace`.
                        _ => {
                            acc.parts.insert(part_index, PartAccum::Whole(part.clone()));
                        }
                    }
                    self.emit(session, EventKind::PartAppend { part_index, part })
                        .await?;
                }
                Ok(ModelDelta::PartReplace { part_index, part }) => {
                    acc.produced = true;
                    acc.parts.insert(part_index, PartAccum::Whole(part.clone()));
                    self.emit(session, EventKind::PartReplace { part_index, part })
                        .await?;
                }
                Ok(ModelDelta::Metadata {
                    response_id,
                    request_id,
                }) => {
                    // Metadata about what answered, known once the response is identified. It does
                    // not change the answer, but it is how a caller correlates it with the provider's
                    // logs, so it is recorded with the turn rather than dropped.
                    acc.response_id = response_id;
                    acc.request_id = request_id;
                }
                Ok(ModelDelta::ToolCallStarted {
                    index,
                    part_index,
                    id,
                    name,
                }) => {
                    acc.produced = true;
                    let entry = acc.calls.entry(index).or_default();
                    entry.id = id.clone();
                    entry.name = name.clone();
                    acc.call_parts.insert(part_index, index);
                    self.emit(
                        session,
                        EventKind::ToolCallStarted {
                            index,
                            part_index,
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
                    match &mut acc.usage {
                        None => acc.usage = Some(usage.clone()),
                        Some(total) => total.add(&usage),
                    }
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
                // The guard recorded the reason; the ending reads it back rather than keeping a
                // second copy that could disagree with what was validated.
                Ok(ModelDelta::Finished { .. }) => {}
                Err(error) => {
                    // Whatever the stream produced before it broke is already the answer a reader has
                    // seen; it goes to the store before the failure is recorded.
                    self.flush(session).await?;
                    return Ok(Ending::Failed {
                        error,
                        usage: acc.usage.unwrap_or_default(),
                        produced: acc.produced,
                        degraded,
                        phase: AttemptPhase::Execute,
                    });
                }
            }
        }
        // The stream ended: the last fragments are not waiting for an event that will never come.
        self.flush(session).await?;

        let usage = acc.usage.take().unwrap_or_default();
        // Taken before `finish` consumes the accumulator: metadata is a fact about the response, and
        // a part of the turn even when the parts are empty.
        let response_id = acc.response_id.take();
        let request_id = acc.request_id.take();
        let produced = acc.produced;

        // A stream that ends without a finish reason did not finish. Reading that as `Stop` files a
        // cut connection as a complete answer, and the reader has no way to tell a short answer from
        // a truncated one — so the turn fails. What already arrived stays in the log: the provider
        // billed it and a client has rendered it, and deleting it would make the record disagree
        // with what the caller saw.
        //
        // Whether this is retried is not decided here: a truncated attempt that produced content is
        // never retried (two partial answers in one turn is worse than one), and one that produced
        // nothing is retried like any other attempt that owes nothing.
        let Some(finish) = guard.finish().cloned() else {
            return Ok(Ending::Failed {
                error: Error::new(
                    crate::contract::ErrorCode::ProviderTruncated,
                    "the model stopped streaming without saying why: the stream ended between                      fragments. What arrived is in the log",
                ),
                usage,
                produced,
                degraded,
                phase: AttemptPhase::Execute,
            });
        };

        let message = acc.finish()?;
        // An answer with nothing in it is not an answer. A successful empty turn is
        // indistinguishable from a model that had nothing to say, so a provider filter that fired,
        // a proxy that ate the body and a decoder that recognised nothing all look like success.
        if message.content.parts.is_empty() {
            return Ok(Ending::Failed {
                error: Error::new(
                    crate::contract::ErrorCode::ProviderResponseUnusable,
                    format!(
                        "the model finished with {finish:?} and produced no content: an empty turn is \
                         not an answer"
                    ),
                ),
                usage,
                produced,
                degraded,
                phase: AttemptPhase::Execute,
            });
        }

        Ok(Ending::Answered {
            message,
            finish,
            usage,
            degraded,
            response_id,
            request_id,
        })
    }

    /// What this turn is asking for.
    ///
    /// The messages come from the caller, not from the session: an inference is a pure call —
    /// what you send is what you get, and whether the answer enters history is the caller's
    /// decision. That is what makes a verification turn possible without polluting the transcript.
    fn build_request(
        &self,
        session: &Session,
        messages: &[Message],
        overrides: &crate::kernel::InferenceOverrides,
    ) -> ModelRequest {
        ModelRequest {
            // Per-call patch over session defaults: only the fields the caller set change anything.
            system: overrides.system.clone().or_else(|| session.system.clone()),
            messages: messages.into(),
            tools: match &overrides.tools {
                Some(tools) => tools.clone(),
                None => self.tool_specs(session),
            },
            params: match &overrides.params {
                Some(params) => session.params.merged(params),
                None => session.params.clone(),
            },
            hints: match &overrides.hints {
                Some(hints) => session.hints.merged(hints),
                None => session.hints.clone(),
            },
            output: overrides.output,
            purpose: Purpose::Answer,
            extensions: overrides.extensions.clone(),
            // Filled here, where the session and the caller are both known. An adapter may derive
            // provider correlation from it; the kernel never reads it back.
            correlation: Some(Arc::new(crate::adapter::Correlation {
                session: session.id.to_string(),
                turn: session.steps,
                tenant: session.principal.tenant.clone(),
                subject: session.principal.subject.clone(),
            })),
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

/// Append `from`'s bytes to `into`, when both are the same modality carried inline.
///
/// Returns whether it merged: a URL source, a different media type, or a mismatch between the two
/// kinds is not an append, and the caller replaces instead. Each modality appends through its own
/// source type, so a clip cannot be merged into an image — the types say so before this runs.
fn append_media(into: &mut Part, from: &Part) -> bool {
    match (&mut into.kind, &from.kind) {
        (PartKind::Image(target), PartKind::Image(next)) => target.source.append(&next.source),
        (PartKind::Audio(target), PartKind::Audio(next)) => target.source.append(&next.source),
        (PartKind::Video(target), PartKind::Video(next)) => target.source.append(&next.source),
        _ => false,
    }
}

/// Whether an adapter accounted for exactly what the request asked for.
///
/// Two failures, both the adapter's: it dropped a field the caller set, or it claims a field the
/// caller never set. Either way the answer would not be the answer to the question, and a call that
/// succeeds while ignoring its input is worse than one that fails.
fn contract(
    model: &Arc<dyn Model>,
    requested: &[crate::contract::Field],
    plan: &crate::adapter::Plan,
) -> Result<()> {
    use std::collections::BTreeSet;

    // A degradation is a report, and a report with no explanation is not one. Treat it as the
    // adapter's contract failure rather than recording an empty reason that a caller cannot act on.
    for entry in &plan.degraded {
        if let Err(reason) = entry.validate() {
            return Err(Error::new(
                crate::contract::ErrorCode::AdapterContractViolation,
                reason,
            ));
        }
    }

    // An extension for another provider is inert, not an error: a caller may carry options for
    // several providers at once, and each attempt applies only its own. So a foreign extension is
    // excluded from *both* sides of the comparison — expecting it in `honoured` would make every
    // multi-provider request fail, and accepting it there would let an adapter claim a knob nobody
    // asked it about.
    let provider = model.provider_id();
    let own = |field: &crate::contract::Field| match field {
        crate::contract::Field::Extension {
            provider: other, ..
        } => other == provider,
        _ => true,
    };

    let requested: BTreeSet<_> = requested
        .iter()
        .filter(|field| own(field))
        .cloned()
        .collect();
    let mut explained: BTreeSet<_> = plan
        .honoured
        .iter()
        .filter(|field| own(field))
        .cloned()
        .collect();
    explained.extend(
        plan.degraded
            .iter()
            .map(|degraded| degraded.field.clone())
            .filter(own),
    );

    if let Some(missing) = requested.difference(&explained).next() {
        return Err(Error::new(
            crate::contract::ErrorCode::AdapterContractViolation,
            format!(
                "the model adapter neither honoured nor explained `{missing}`, which the request set: \
                 an option that silently does nothing is a session behaving differently from how it was \
                 configured"
            ),
        ));
    }
    if let Some(invented) = explained.difference(&requested).next() {
        return Err(Error::new(
            crate::contract::ErrorCode::AdapterContractViolation,
            format!("the model adapter claims `{invented}`, which the request never set"),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::contract::{AudioSource, ImageSource, VideoSource};

    fn image_bytes(bytes: &[u8]) -> Part {
        Part::new(PartKind::Image(crate::contract::ImagePart {
            source: ImageSource::data(bytes.to_vec(), "image/png").unwrap(),
        }))
    }

    fn audio_bytes(bytes: &[u8]) -> Part {
        Part::new(PartKind::Audio(crate::contract::AudioPart {
            source: AudioSource::data(bytes.to_vec(), "audio/wav").unwrap(),
            format: None,
            duration_millis: None,
        }))
    }

    fn video_bytes(bytes: &[u8]) -> Part {
        Part::new(PartKind::Video(crate::contract::VideoPart {
            source: VideoSource::data(bytes.to_vec(), "video/mp4").unwrap(),
            duration_millis: None,
        }))
    }

    #[test]
    fn two_inline_images_append() {
        let mut into = image_bytes(&[1, 2]);
        let from = image_bytes(&[3, 4]);
        assert!(append_media(&mut into, &from));
        let PartKind::Image(image) = &into.kind else {
            panic!("still an image");
        };
        assert_eq!(image.source.bytes(), Some(&[1, 2, 3, 4][..]));
    }

    #[test]
    fn two_inline_audio_clips_append() {
        let mut into = audio_bytes(&[1]);
        let from = audio_bytes(&[2]);
        assert!(append_media(&mut into, &from));
    }

    #[test]
    fn two_inline_videos_append() {
        let mut into = video_bytes(&[1]);
        let from = video_bytes(&[2]);
        assert!(append_media(&mut into, &from));
    }

    #[test]
    fn an_image_cannot_be_appended_into_audio() {
        let mut into = audio_bytes(&[1]);
        let from = image_bytes(&[2]);
        assert!(
            !append_media(&mut into, &from),
            "different modalities do not merge"
        );
    }

    #[test]
    fn text_does_not_append() {
        let mut into = Part::text("hello");
        let from = Part::text(" world");
        assert!(
            !append_media(&mut into, &from),
            "text is not a media append"
        );
    }

    #[test]
    fn a_url_source_does_not_append() {
        let mut into = Part::new(PartKind::Image(crate::contract::ImagePart {
            source: ImageSource::url("https://example.com/a.png", "image/png").unwrap(),
        }));
        let from = image_bytes(&[1]);
        assert!(
            !append_media(&mut into, &from),
            "a URL is not bytes to extend"
        );
    }

    #[test]
    fn a_different_media_type_does_not_append() {
        let mut into = image_bytes(&[1]);
        let from = Part::new(PartKind::Image(crate::contract::ImagePart {
            source: ImageSource::data(vec![2], "image/jpeg").unwrap(),
        }));
        assert!(
            !append_media(&mut into, &from),
            "image/png and image/jpeg are not the same stream"
        );
    }
}
