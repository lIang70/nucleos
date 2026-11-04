//! Bringing a session up to the log's head after it was reloaded.
//!
//! A checkpoint is a cache of the log, not a replacement for it. The kernel writes one at turn
//! boundaries, so a process that dies mid-turn leaves events *after* the last checkpoint: the tokens
//! it paid for, the text it had already streamed, the tool call it had started. Loading only the
//! snapshot threw all of that away — which is why a crash used to lose paid work.
//!
//! So `load` folds the tail. Every event kind is classified explicitly, and the classification is
//! the design:
//!
//! - **State** — the event changed something the session carries, so it is applied.
//! - **Narrative** — the event describes what a consumer saw, not what the session is. Deltas are
//!   the stream a client renders; replaying them into state would rebuild a message the model never
//!   finished. They stay in the log, where a caller can read them.
//!
//! The catch-all is absent on purpose: a new event kind that nothing classified is a compile error,
//! not a silent no-op. That is the failure this module exists to avoid, one layer down.
//!
//! ## What a half-finished turn becomes
//!
//! It is not resumed (no provider can resume a stream) and it is not silently dropped. The session
//! records an `attempt_failed` naming the reason, keeps the spend, and does not pretend the message
//! completed — the model's next turn does not see a fragment it never finished writing.

use super::session::Session;
use crate::contract::Message;
use crate::contract::{
    Attempt, AttemptOutcome, AttemptPhase, AttemptTrigger, Event, EventKind, Role,
};

/// What folding one event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    /// It changed session state.
    State,
    /// It describes what happened, not what the session is.
    Narrative,
    /// It arrived mid-turn and the turn never finished.
    AbandonedTurn,
}

/// Fold one event into a loaded session.
pub(crate) fn fold(session: &mut Session, event: &Event) -> Applied {
    match &event.kind {
        EventKind::StateChanged { state, stop } => {
            session.state = *state;
            session.stop = *stop;
            Applied::State
        }
        EventKind::SessionConfigured { patch } => {
            // The patch was validated when it was applied, and re-validating would re-run
            // `configure` for a change that already happened. The fields it sets are restored
            // directly; anything a patch could not have changed is in the snapshot already.
            if let Some(system) = &patch.system {
                session.system = system.clone();
            }
            if let Some(params) = &patch.params {
                session.params = params.clone();
            }
            if let Some(hints) = &patch.hints {
                session.hints = session.hints.merged(hints);
            }
            if let Some(tools) = &patch.tools {
                session.tools = tools.clone();
            }
            if let Some(policy) = &patch.tool_policy {
                session.tool_policy = policy.clone();
            }
            if let Some(sandbox) = &patch.sandbox {
                session.sandbox = sandbox.clone();
            }
            if let Some(routing) = &patch.routing {
                session.routing = routing.clone();
                session.retries = routing.retries;
            }
            Applied::State
        }
        EventKind::Usage {
            usage,
            model,
            attempt_index,
            ..
        } => {
            // The money was spent whether or not the turn finished. Rebuilding the session without
            // it would hand back credit that is already gone.
            session.usage.add(usage);
            if let Some(credit) = session.credit_tokens {
                session.credit_tokens = Some(credit.saturating_sub(usage.total_tokens()));
            }
            session.ledger.total.add(usage);

            // A stream reports usage more than once for one attempt, so the ledger is keyed by the
            // attempt: folding each report into its own entry would turn one bill into eight.
            match session
                .ledger
                .attempts
                .iter_mut()
                .find(|attempt| attempt.index == *attempt_index)
            {
                Some(attempt) => attempt.usage.add(usage),
                None => session.ledger.attempts.push(Attempt {
                    index: *attempt_index,
                    model: model.clone(),
                    phase: AttemptPhase::Execute,
                    trigger: AttemptTrigger::Selection,
                    outcome: AttemptOutcome::Ok,
                    usage: usage.clone(),
                    duration_millis: 0,
                    degraded: Vec::new(),
                }),
            }
            Applied::State
        }
        EventKind::AttemptFailed { attempt } => {
            session.ledger.record(attempt.clone());
            Applied::State
        }
        EventKind::MessageCompleted {
            message,
            step_index,
        } => {
            session.messages.push(message.clone());
            // The step counter is a budget, and the checkpoint may be older than this event: a turn
            // that completed is a step the session took, so a reload must not hand it back. The 1-based
            // step index the event carries is the same number `spend` charges against.
            session.steps = session.steps.max(u64::from(*step_index));
            Applied::State
        }
        EventKind::ToolFinished { result, usage, .. } => {
            if let Some(usage) = usage {
                session.usage.add(usage);
                if let Some(credit) = session.credit_tokens {
                    session.credit_tokens = Some(credit.saturating_sub(usage.total_tokens()));
                }
            }
            session.messages.push(Message {
                role: Role::Tool,
                content: crate::contract::Content::new([crate::contract::Part::new(
                    crate::contract::PartKind::ToolResult(crate::contract::ToolResultPart {
                        result: result.clone(),
                    }),
                )]),
            });
            Applied::State
        }
        EventKind::GenerationMigrated { to, .. } => {
            session.generation = *to;
            Applied::State
        }
        EventKind::ToolsExposed { names } => {
            for name in names {
                session.discovered.insert(name.clone(), session.round);
            }
            Applied::State
        }
        EventKind::BudgetExhausted { reason } => {
            session.stop = Some(*reason);
            Applied::State
        }
        EventKind::Credited { remaining, .. } => {
            // A grant is a state change: the balance is what the event says it is, and a session that
            // stopped on a budget is open again. Other stop reasons are not revived — a completed or
            // cancelled session is done, not out of money.
            session.credit_tokens = *remaining;
            if session.stop.is_some_and(|stop| {
                matches!(
                    stop,
                    crate::contract::StopReason::Tokens
                        | crate::contract::StopReason::Cost
                        | crate::contract::StopReason::Steps
                        | crate::contract::StopReason::Wall
                )
            }) {
                session.state = crate::contract::SessionState::Open;
                session.stop = None;
            }
            Applied::State
        }
        EventKind::TurnFailed { .. } => Applied::Narrative,
        // Written by a build that knows more than this one. It is narrative by definition: nothing
        // here can say what state it implies, and guessing is how a downgrade corrupts a session.
        EventKind::Unknown => Applied::Narrative,
        EventKind::Failed { .. } => {
            session.stop = Some(crate::contract::StopReason::Failed);
            Applied::State
        }
        // The parked call is in the snapshot, and a park writes its own checkpoint, so a parked
        // session cannot be one whose tail needs the pending call rebuilt.
        EventKind::AwaitingHuman { .. } => Applied::Narrative,
        EventKind::ToolCallStarted { .. }
        | EventKind::ToolCallArgs { .. }
        | EventKind::TurnStarted { .. } => {
            // A call that started and never finished, or a turn that began and never ended: the turn
            // it belonged to is the one that was abandoned, so this is evidence of that rather than
            // state of its own. `turn_started` is on the list because a crash before the first delta
            // otherwise leaves nothing at all — a turn that spent money and wrote no record of it.
            Applied::AbandonedTurn
        }
        EventKind::TextDelta { .. }
        | EventKind::ReasoningDelta { .. }
        | EventKind::CapabilityDegraded(_)
        | EventKind::Progress { .. }
        | EventKind::ExecStarted { .. }
        | EventKind::ExecOutput { .. }
        | EventKind::ExecStdin { .. }
        | EventKind::ExecExited { .. } => Applied::Narrative,
    }
}

/// Whether an event ends the fragment a reader is reassembling.
///
/// One function, because two readers need it: `partial` walks forward and resets on these, and the
/// kernel now walks *backwards* from the head and stops on them. Two lists would drift, and the
/// failure would be a fragment that includes a previous turn.
pub(crate) fn ends_the_fragment(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::MessageCompleted { .. }
            | EventKind::ToolFinished { .. }
            | EventKind::StateChanged { .. }
    )
}

/// Reassemble what the assistant had produced in a turn that never finished.
///
/// Reads the log rather than the session: the fragment is deliberately not in `messages`, because a
/// half-written message is not something the model should reason from. This is how a caller finds out
/// what it said before it stopped, without walking SSE events itself.
pub(crate) fn partial(events: &[Event]) -> Option<crate::contract::PartialTurn> {
    let mut partial = crate::contract::PartialTurn::default();
    let mut call: Option<crate::contract::UnfinishedCall> = None;
    let mut saw_anything = false;

    for event in events {
        // A completed message, a finished tool or a state change ends the fragment. `attempt_failed`
        // deliberately does not: an attempt that streamed and then died left exactly the fragment
        // this function is for, and the record of the abandonment must not erase what it announces.
        if ends_the_fragment(&event.kind) {
            partial = crate::contract::PartialTurn::default();
            call = None;
            saw_anything = false;
            continue;
        }
        match &event.kind {
            EventKind::TextDelta { text, .. } => {
                partial.text.push_str(text);
                saw_anything = true;
            }
            EventKind::ToolCallStarted {
                tool_call_id, name, ..
            } => {
                call = Some(crate::contract::UnfinishedCall {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments_json: String::new(),
                });
                saw_anything = true;
            }
            EventKind::ToolCallArgs { fragment, .. } => {
                if let Some(call) = call.as_mut() {
                    call.arguments_json.push_str(fragment);
                }
            }
            _ => {}
        }
    }

    partial.unfinished_call = call;
    saw_anything.then_some(partial)
}

/// Whether an event completes a unit of work, and therefore ends any earlier evidence of a turn that
/// did not.
///
/// This is what makes recovery idempotent: once `load` has recorded the abandoned turn, the record
/// itself is such an event, so a second load finds no evidence left to infer and appends nothing.
pub(crate) fn completes_a_unit(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::MessageCompleted { .. }
            | EventKind::ToolFinished { .. }
            | EventKind::AttemptFailed { .. }
            // A turn that ended by writing down why is a finished turn, not an abandoned one. Without
            // this, recovery reads `turn_started` as evidence, finds `turn_failed` beneath it, and
            // appends an abandoned attempt — inventing a model and a duration for a turn that already
            // said what happened.
            | EventKind::TurnFailed { .. }
            | EventKind::Failed { .. }
            | EventKind::StateChanged { .. }
    )
}

/// The reason recorded for a turn the runtime did not survive.
pub(crate) const ABANDONED: &str = "the runtime stopped mid-turn; the spend is kept, the partial \
                                     output stays in the stream, and the message is not completed";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ErrorCode;

    /// The pairing that makes recovery idempotent: an announcement is evidence, its ending retires it.
    #[test]
    fn every_ending_retires_the_evidence_a_turn_began() {
        assert!(!completes_a_unit(&EventKind::TurnStarted {
            model: "primary".to_owned(),
            generation: 1,
        }));
        for ending in [
            EventKind::MessageCompleted {
                message: crate::contract::Message {
                    role: crate::contract::Role::Assistant,
                    content: crate::contract::Content::default(),
                },
                step_index: 0,
            },
            EventKind::AttemptFailed {
                attempt: crate::contract::Attempt {
                    index: 0,
                    model: "primary".to_owned(),
                    phase: crate::contract::AttemptPhase::Execute,
                    trigger: crate::contract::AttemptTrigger::Selection,
                    outcome: crate::contract::AttemptOutcome::Failed {
                        code: ErrorCode::Unavailable,
                        reason: "down".to_owned(),
                    },
                    usage: crate::contract::Usage::default(),
                    duration_millis: 0,
                    degraded: Vec::new(),
                },
            },
            EventKind::TurnFailed {
                code: ErrorCode::Denied,
                reason: "no".to_owned(),
            },
            EventKind::Failed {
                code: ErrorCode::Internal,
                reason: "it broke".to_owned(),
            },
        ] {
            assert!(
                completes_a_unit(&ending),
                "{} must end a turn, or recovery invents an abandoned one",
                ending.type_name()
            );
        }
    }
}
