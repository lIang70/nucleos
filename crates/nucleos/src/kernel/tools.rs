//! Tool calls, and the gate in front of them.
//!
//! Policy is asked about a whole batch before any of it runs, and a verdict that has already been
//! answered is not asked again — `invoke_approved` is the one door that skips the gate.

use super::{Kernel, session};
use crate::adapter::Tool;
use crate::adapter::ToolOutput;
use crate::contract::{Content, Exposure, Message, Part, PartKind};
use crate::contract::{EventKind, ToolResultPart};
use crate::error::{Error, Result};
use std::sync::Arc;

impl Kernel {
    /// Ask policy about one call.
    pub async fn check(
        &self,
        session: &session::Session,
        call: &crate::contract::ToolCall,
    ) -> crate::adapter::Verdict {
        self.policy()
            .check(crate::adapter::PolicyRequest {
                principal: &session.principal,
                session: Some(&session.id),
                subject: crate::adapter::Gated::Tool { call },
            })
            .await
    }
    /// Run one tool call.
    ///
    /// Policy is consulted here as well as in [`Kernel::check`]: a kernel that executed an ungated
    /// tool because a caller forgot to ask first would not be a kernel.
    pub async fn invoke_tool(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
    ) -> Result<ToolOutput> {
        let mut outputs = self
            .invoke_tools(session, std::slice::from_ref(call))
            .await?;
        outputs
            .pop()
            .ok_or_else(|| Error::internal("a single call produced no result"))
    }
    /// Run a batch of calls.
    ///
    /// Policy is asked about every call before any runs, so a batch that needs a human parks with
    /// nothing executed. Then the allowed ones run concurrently and their results are folded back
    /// in order — state writes serial, tool execution parallel, which are different axes.
    pub async fn invoke_tools(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        // A tool may report a cost, and a session with no room left must not start one. The gate is
        // here rather than in the driver for the same reason the model gate is: a driver that
        // forgets to ask is a driver that spends.
        self.spend(session).await?;

        let mut verdicts = Vec::with_capacity(calls.len());
        for call in calls {
            let verdict = self.check(session, call).await;
            tracing::debug!(
                event = "tool_gated",
                session = %session.id,
                tool = %call.name,
                verdict = match &verdict {
                    crate::adapter::Verdict::Allow => "allow",
                    crate::adapter::Verdict::Deny { .. } => "deny",
                    crate::adapter::Verdict::AskHuman { .. } => "ask",
                },
            );
            match verdict {
                crate::adapter::Verdict::AskHuman { question, options } => {
                    self.park(
                        session,
                        crate::contract::PendingCall {
                            call: call.clone(),
                            options,
                            question,
                        },
                    )
                    .await?;
                    return Err(Error::unavailable(
                        "policy asked a human; the batch is parked with nothing executed",
                    ));
                }
                crate::adapter::Verdict::Allow => verdicts.push(None),
                crate::adapter::Verdict::Deny { reason, .. } => verdicts.push(Some(reason)),
            }
        }
        self.execute_calls(session, calls, verdicts).await
    }
    /// Run calls that are already allowed.
    ///
    /// A human's verdict *is* the policy answer; asking again would ask the same question
    /// forever. This is the only door that skips the gate, and it is not reachable from the wire.
    pub async fn invoke_approved(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        let verdicts = vec![None; calls.len()];
        self.execute_calls(session, calls, verdicts).await
    }

    async fn execute_calls(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
        verdicts: Vec<Option<String>>,
    ) -> Result<Vec<ToolOutput>> {
        let deadline = self.deadline(session);
        let session_id = session.id.clone();
        let started = self.now();

        // Progress is written by a task, not by the calls: the session is borrowed here and the
        // batch runs concurrently, so a tool that waited for `&mut Session` to report would block the
        // very work it is describing. One writer, one channel, then nothing until the batch ends.
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<Reported>(64);
        let writer = super::writer::EventWriter::new(
            Arc::clone(&self.store),
            session.id.clone(),
            Arc::clone(&session.seq),
            Arc::clone(&self.clock),
        );
        let progress_writer = tokio::spawn(async move {
            while let Some(report) = progress_rx.recv().await {
                let _ = progress_writer_append(&writer, report).await;
            }
        });
        // Resolved once, against the generation this session is pinned to: a call is answered by the
        // set that built the prompt, even if a newer generation has been published since.
        let resolved: Vec<Option<Arc<dyn Tool>>> = calls
            .iter()
            .map(|call| self.tool_in(session, &call.name))
            .collect();

        let futures = calls
            .iter()
            .zip(verdicts)
            .zip(resolved)
            .map(|((call, denial), tool)| {
                let progress_tx = progress_tx.clone();
            let args = call.arguments_json.clone();
            let call_id = call.id.clone();
            let session_id = session_id.clone();
            async move {
                let sink = Sink {
                    call_id: call_id.clone(),
                    sender: progress_tx,
                };
                let ctx = crate::adapter::ToolCtx {
                    session: &session_id,
                    call_id: &call_id,
                    deadline_unix_millis: deadline,
                    progress: &sink,
                };
                match denial {
                    None => match tool {
                        // A model that produced broken JSON gets told so, and the turn
                        // continues. Executing it would mean guessing at the arguments.
                        Some(tool) => match serde_json::from_str::<serde_json::Value>(&args) {
                            Ok(_) => tool.invoke(&args, &ctx).await,
                            Err(error) => ToolOutput::failed(format!(
                                "arguments were not valid JSON: {error}"
                            )),
                        },
                        None => ToolOutput::failed(format!(
                            "no tool named {:?} in this session's tool generation",
                            call.name
                        )),
                    },
                    Some(reason) => ToolOutput::failed(reason),
                }
            }
        });
        let outputs = futures_util::future::join_all(futures).await;
        // Closing the channel is what ends the writer: awaiting it afterwards means every progress
        // event a tool reported is in the log before the caller sees the results.
        drop(progress_tx);
        let _ = progress_writer.await;
        let duration = self.now().saturating_sub(started);

        for (call, output) in calls.iter().zip(outputs.iter()) {
            self.record_call(session, call, output.clone(), duration)
                .await?;
            self.expose(session, &output.expose).await?;
        }
        self.write_checkpoint(session).await?;
        Ok(outputs)
    }
    /// Record a refusal as the call's result.
    ///
    /// A refusal is content, not a failure of the session: the model reads why and decides what
    /// to do next.
    pub async fn record_refusal(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.record_call(session, call, ToolOutput::failed(reason), 0)
            .await
    }

    /// Record a call's outcome: the event, the spend, and the message that enters history.
    async fn record_call(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        output: ToolOutput,
        duration_millis: u64,
    ) -> Result<()> {
        // Using a tool is the strongest evidence it belongs in the pool: the model reached for it
        // without searching first, which is what "recently used" is for. The search tool itself is
        // always visible, so giving it a slot would evict a found tool to remember a door.
        if self.allowable(session, &call.name) && call.name != super::tool_search::NAME {
            session.discovered.insert(call.name.clone(), session.round);
        }
        // A tool that reported a cost is charged for it; one that said nothing is not assumed
        // free. The ledger's rule is the same one model usage follows.
        if let Some(usage) = &output.usage {
            session.usage.add(usage);
            session.credit_tokens = session.credit_tokens.saturating_sub(usage.total_tokens());
        }
        let result = crate::contract::ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        };
        if output.is_error {
            // The tool's own words go to the model; the operator gets the fact and the identity.
            tracing::debug!(
                event = "tool_failed",
                session = %session.id,
                tool = %call.name,
                call = %call.id,
            );
        }
        self.emit(
            session,
            EventKind::ToolFinished {
                duration_millis,
                result: result.clone(),
                usage: output.usage,
            },
        )
        .await?;
        session.messages.push(Message {
            role: crate::contract::Role::Tool,
            content: Content::new([Part::new(PartKind::ToolResult(ToolResultPart { result }))]),
        });
        Ok(())
    }
    /// Widen what the model can see, as far as the ceiling allows.
    ///
    /// A tool may *ask* — it has no handle on the session — and the kernel answers. Names outside
    /// the ceiling are dropped rather than refused: a search tool that ranked a forbidden tool
    /// should not turn the whole call into an error, and showing a tool that may never run would
    /// provoke a call whose refusal the model cannot explain.
    async fn expose(&self, session: &mut session::Session, names: &[String]) -> Result<()> {
        let before = self.shown(session);
        for name in names {
            // The ceiling decides, not the tool: this is the whole reason exposure is safe. A tool
            // that ranked something forbidden is not an error — it has no session and cannot know.
            if !self.allowable(session, name) || self.level(session, name) == Exposure::Hidden {
                continue;
            }
            session.discovered.insert(name.clone(), session.round);
        }
        self.trim_pool(session);
        let after = self.shown(session);

        // What reached the prompt, not what entered the pool. A discovery the budget cannot fit is
        // still callable by name — the prompt is a view of the pool, not the whole of it — but an
        // event claiming it entered the prompt would be a second, wrong truth.
        let added: Vec<String> = after.difference(&before).cloned().collect();
        if !added.is_empty() {
            self.emit(session, EventKind::ToolsExposed { names: added })
                .await?;
        }
        Ok(())
    }

    /// The names a prompt would carry right now, budget included.
    #[must_use]
    pub fn shown(&self, session: &session::Session) -> std::collections::BTreeSet<String> {
        self.tool_specs(session)
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    /// Whether the ceiling allows a tool at all.
    #[must_use]
    pub(crate) fn allowable(&self, session: &session::Session, name: &str) -> bool {
        let generations = self.generations();
        let set = generations.set(session.generation);
        set.contains_key(name) && (session.tools.is_empty() || session.tools.contains(&name.to_owned()))
    }

    /// How visible a tool is under the session's policy.
    #[must_use]
    pub(crate) fn level(&self, session: &session::Session, name: &str) -> Exposure {
        session.tool_policy.exposure_of(name)
    }

    /// Forget what has not been reached recently, and what no longer fits.
    ///
    /// Expiry is what keeps the pool from growing into "everything, eventually": a tool found on
    /// turn 3 and never used again should not still be displacing a baseline on turn 40.
    pub(crate) fn trim_pool(&self, session: &mut session::Session) {
        let policy = session.tool_policy.discovery.clone();
        let round = session.round;
        // Expiry only: what the ceiling allows is settled when a name enters the pool, and
        // re-checking it here would need the ceiling while the pool is borrowed.
        session
            .discovered
            .retain(|_name, last| round.saturating_sub(*last) < policy.idle_rounds);

        if session.discovered.len() <= policy.max_tools {
            return;
        }
        // Evict by rank first, then by staleness: a baseline is never evicted, and among equals the
        // one that has gone longest without use goes first.
        let mut entries: Vec<(String, u64)> = session
            .discovered
            .iter()
            .map(|(name, last)| (name.clone(), *last))
            .collect();
        entries.sort_by(|left, right| {
            let left_rank = session.tool_policy.exposure_of(&left.0).rank();
            let right_rank = session.tool_policy.exposure_of(&right.0).rank();
            right_rank.cmp(&left_rank).then(left.1.cmp(&right.1))
        });
        for (name, _) in entries {
            if session.discovered.len() <= policy.max_tools {
                break;
            }
            session.discovered.remove(&name);
        }
    }

    /// Advance the turn counter and expire what has gone quiet.
    pub(crate) fn begin_round(&self, session: &mut session::Session) {
        session.round = session.round.saturating_add(1);
        self.trim_pool(session);
    }

    /// One tool by name, in the current generation.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.generations().current_set().get(name).cloned()
    }

    /// One tool by name, in the generation this session is pinned to.
    ///
    /// The distinction is the whole point of pinning: the model produced a call from a prompt built
    /// out of one set, so that set is what answers it — even if a newer one has been published and
    /// even if the tool has since been withdrawn.
    #[must_use]
    pub fn tool_in(&self, session: &session::Session, name: &str) -> Option<Arc<dyn Tool>> {
        self.generations().set(session.generation).get(name).cloned()
    }

    /// The declarations this session may use, in name order so a prompt is reproducible.
    ///
    /// The ceiling decides what may ever be used; the policy and the pool decide what is in the
    /// prompt now; the budget decides how much of that fits. All three, in that order.
    #[must_use]
    pub fn tool_specs(&self, session: &session::Session) -> Vec<crate::contract::ToolSpec> {
        // The door is shown only when something is behind it. A policy that shows every tool in
        // full leaves nothing to find, so spending a schema on the way to find it would be paying
        // for an empty room.
        let generations = self.generations();
        let set = generations.set(session.generation);
        let needs_door = set
            .keys()
            .filter(|name| self.allowable(session, name))
            .any(|name| session.tool_policy.exposure_of(name).searchable());

        let mut candidates: Vec<(crate::contract::Exposure, u64, crate::contract::ToolSpec)> = set
            .iter()
            .filter(|(name, _)| session.tools.is_empty() || session.tools.contains(name))
            .filter_map(|(name, tool)| {
                let exposure = if *name == super::tool_search::NAME && !needs_door {
                    crate::contract::Exposure::Hidden
                } else {
                    session.tool_policy.exposure_of(name)
                };
                let last_used = session.discovered.get(name).copied();
                let shown = match exposure {
                    crate::contract::Exposure::Always => true,
                    crate::contract::Exposure::Direct | crate::contract::Exposure::Deferred => {
                        last_used.is_some()
                    }
                    crate::contract::Exposure::Hidden => false,
                };
                shown.then(|| (exposure, last_used.unwrap_or(0), tool.spec()))
            })
            .collect();

        // Stable order before pruning: baseline first, then the most recently reached, then name —
        // so two sessions with the same history show the same prompt.
        candidates.sort_by(|left, right| {
            left.0
                .rank()
                .cmp(&right.0.rank())
                .then(right.1.cmp(&left.1))
                .then(left.2.name.cmp(&right.2.name))
        });

        let budget = &session.tool_policy.budget;
        let mut bytes = 0u64;
        let mut specs = Vec::new();
        for (exposure, _, spec) in candidates {
            let weight = definition_bytes(&spec);
            // The baseline is admitted whatever the budget says. An application's own `always` set
            // disappearing because discovery used the room would be the configuration being
            // ignored by the thing that was supposed to enforce it — and a prompt that loses its
            // door has no way to ask for it back.
            if exposure == Exposure::Always {
                bytes += weight;
                specs.push(spec);
                continue;
            }
            // The count is monotonic along the sorted order, so the first overflow ends the list.
            if specs.len() >= budget.max_definitions {
                break;
            }
            // Bytes are not: one enormous schema early in the order would otherwise evict every
            // smaller tool behind it. Skip this one and keep filling.
            if bytes + weight > budget.max_bytes {
                continue;
            }
            bytes += weight;
            specs.push(spec);
        }
        // Name order for the prompt itself, which is what a replay compares against.
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    /// Whether a discovery tool exists at all.
    #[must_use]
    pub fn discoverable(&self) -> bool {
        self.generations()
            .current_set()
            .contains_key(super::tool_search::NAME)
    }
}

/// What one definition costs, approximated without serializing it.
///
/// The constant stands in for the punctuation and the field names around the three strings; it
/// exists so pruning does not allocate a JSON document per tool per turn.
fn definition_bytes(spec: &crate::contract::ToolSpec) -> u64 {
    (spec.name.len() + spec.description.len() + spec.input_schema_json.len() + 32) as u64
}

/// One progress report, on its way to the log.
struct Reported {
    token: String,
    done: u64,
    total: u64,
    message: String,
}

/// The sink a tool holds: it can only send, so a tool cannot write events itself.
struct Sink {
    call_id: String,
    sender: tokio::sync::mpsc::Sender<Reported>,
}

impl crate::adapter::Progress for Sink {
    fn report(&self, done: u64, total: u64, message: &str) {
        // A full channel drops the report rather than blocking the call: a tool that stalls because
        // nobody is reading progress is worse than one whose report is missing.
        let _ = self.sender.try_send(Reported {
            token: self.call_id.clone(),
            done,
            total,
            message: message.to_owned(),
        });
    }
}

/// Write one report as an event.
async fn progress_writer_append(
    writer: &super::writer::EventWriter,
    report: Reported,
) -> Result<()> {
    writer
        .append(EventKind::Progress {
            token: report.token,
            done: report.done,
            total: report.total,
            message: report.message,
        })
        .await
}
