//! The loop itself.

use nucleos::adapter::FinishReason;
use nucleos::contract::{ErrorCode, Principal};
use nucleos::contract::{
    EventKind, Message, PartKind, Role, SessionId, SessionState, StopReason, ToolCall,
};
use nucleos::kernel::{HitlVerdict, Kernel, Outcome, Session, SessionSpec, StateWrite};
use nucleos::{Error, Result};

/// Open a session and drive it until it stops.
pub async fn start(
    kernel: &Kernel,
    principal: &Principal,
    id: SessionId,
    spec: SessionSpec,
) -> Result<Outcome> {
    let mut session = kernel.open(principal, id, spec).await?;
    drive(kernel, &mut session).await
}

/// Load a session from its checkpoint and continue it.
pub async fn resume(kernel: &Kernel, principal: &Principal, id: &SessionId) -> Result<Outcome> {
    let mut session = kernel.load(principal, id).await?;
    drive(kernel, &mut session).await
}

/// Answer the call a session is parked on, then continue.
pub async fn answer(
    kernel: &Kernel,
    principal: &Principal,
    id: &SessionId,
    verdict: HitlVerdict,
) -> Result<Outcome> {
    let mut session = kernel.load(principal, id).await?;
    let pending = session
        .pending()
        .iter()
        .find(|pending| pending.call.id == verdict.tool_call_id)
        .cloned()
        .ok_or_else(|| {
            Error::invalid(format!(
                "session {id} has no call {} waiting on a human",
                verdict.tool_call_id
            ))
        })?;
    let option = pending
        .options
        .iter()
        .find(|option| option.id == verdict.option_id)
        .cloned()
        .ok_or_else(|| Error::invalid(format!("option {:?} was not offered", verdict.option_id)))?;

    // The verdict becomes the call's result: an allowed call runs, a refused one answers with
    // content the model can read.
    let allowed = matches!(
        option.kind,
        nucleos::contract::PermissionKind::AllowOnce
            | nucleos::contract::PermissionKind::AllowAlways
    );
    if allowed {
        // The verdict *is* the policy answer.
        kernel
            .invoke_approved(&mut session, std::slice::from_ref(&pending.call))
            .await?;
    } else {
        kernel
            .record_refusal(&mut session, &pending.call, "refused by a human")
            .await?;
    }
    kernel.checkpoint(&session).await?;
    drive(kernel, &mut session).await
}

/// Run one turn. Returns the outcome without continuing.
///
/// The escape hatch for an application that wants to drive: it calls this in whatever order its
/// own loop requires, and the kernel still owns every invariant inside a turn.
pub async fn step(kernel: &Kernel, session: &mut Session) -> Result<Step> {
    // The kernel enforces its own budget: a caller that never asked would otherwise spend past it,
    // and the event that records the stop would be the caller's claim rather than the kernel's
    // decision. This loop only translates the refusal into its own vocabulary.
    if session.is_closed() {
        return Ok(Step::Stopped(session.stop_reason().unwrap_or(StopReason::Completed)));
    }

    let outstanding = outstanding_calls(session);
    tracing::debug!(
        event = "step",
        session = %session.id(),
        step = session.inference_calls(),
        outstanding_calls = outstanding.len(),
    );
    let result = if outstanding.is_empty() {
        inference_turn(kernel, session).await
    } else {
        tool_turn(kernel, session, &outstanding).await
    };

    match result {
        // A budget refusal is a stop, not a failure. The kernel has already closed the session and
        // recorded why, so relabelling it here would be the driver overriding a decision it did not
        // make — and it was exactly what this loop used to do.
        Err(error) if error.code() == ErrorCode::Budget => {
            tracing::info!(
                event = "session_stopped",
                session = %session.id(),
                stop = ?session.stop_reason(),
            );
            Ok(Step::Stopped(
                session.stop_reason().unwrap_or(StopReason::Tokens),
            ))
        }
        // Anything else ends the *session*, which is an outcome rather than an error of the call:
        // the attempt's own code and reason are already in the log, and `close` records the stop on
        // the state change. Handing back an `Err` here would say the driver's call was wrong when
        // what happened is that the session ended.
        Err(_) => {
            kernel.close(session, StopReason::Failed).await?;
            Ok(Step::Stopped(StopReason::Failed))
        }
        Ok(step) => Ok(step),
    }
}

/// What one turn did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The model answered and asked for nothing; the session is finished.
    Answered,
    /// The model asked for tools, which ran. Another turn is expected.
    ToolsRan,
    /// A human was asked, and nothing ran.
    Parked,
    /// A stop condition fired and the session was closed.
    Stopped(StopReason),
}

/// Drive until a stop condition, a park, or a closed session.
pub async fn drive(kernel: &Kernel, session: &mut Session) -> Result<Outcome> {
    loop {
        match step(kernel, session).await? {
            Step::Parked | Step::Stopped(_) => break,
            Step::Answered | Step::ToolsRan => {}
        }
    }
    outcome(kernel, session).await
}

async fn inference_turn(kernel: &Kernel, session: &mut Session) -> Result<Step> {
    let messages = session.messages().to_vec();
    // Errors propagate: what a failure *means* is decided once, in `step`.
    let inference = kernel.inference(session, &messages).await?;

    // The provider's reason is the first of four stop conditions and the least trustworthy.
    let stop = match inference.finish {
        FinishReason::Length => Some(StopReason::Tokens),
        FinishReason::Filtered => Some(StopReason::Failed),
        _ => None,
    };
    let asked_for_tools = inference
        .message
        .content
        .parts
        .iter()
        .any(|part| matches!(part.kind, PartKind::ToolCall(_)));

    kernel
        .append(
            session,
            StateWrite {
                messages: vec![inference.message],
                ..StateWrite::default()
            },
        )
        .await?;

    if let Some(reason) = stop {
        kernel.close(session, reason).await?;
        return Ok(Step::Stopped(reason));
    }
    if asked_for_tools {
        return Ok(Step::ToolsRan);
    }
    kernel.close(session, StopReason::Completed).await?;
    Ok(Step::Answered)
}

async fn tool_turn(kernel: &Kernel, session: &mut Session, calls: &[ToolCall]) -> Result<Step> {
    match kernel.invoke_tools(session, calls).await {
        Ok(_) => Ok(Step::ToolsRan),
        Err(error) if error.code() == nucleos::contract::ErrorCode::Unavailable => {
            // Nothing ran: the batch was parked on a human.
            Ok(Step::Parked)
        }
        Err(error) => Err(error),
    }
}

/// The calls of the last assistant message that have no result yet.
///
/// Derived from history rather than tracked in a field, so a resumed turn recomputes it — which
/// is what makes answering a parked call idempotent.
#[must_use]
pub fn outstanding_calls(session: &Session) -> Vec<ToolCall> {
    let Some(message) = session.messages().last() else {
        return Vec::new();
    };
    if message.role != Role::Assistant {
        return Vec::new();
    }
    let answered: Vec<&str> = session
        .messages()
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

async fn outcome(kernel: &Kernel, session: &Session) -> Result<Outcome> {
    let events = kernel.events(session.principal(), session.id(), 0).await?;
    let stop = events.iter().rev().find_map(|event| match &event.kind {
        EventKind::StateChanged { stop, .. } => stop.to_owned(),
        _ => None,
    });
    Ok(Outcome {
        session: session.id().clone(),
        state: if session.is_closed() {
            SessionState::Closed
        } else {
            SessionState::Open
        },
        stop,
        ledger: session.ledger().clone(),
        next_seq: session.next_seq(),
    })
}

/// A message with a single text part, for a driver that wants to inject input.
#[must_use]
pub fn text_message(role: Role, text: impl Into<String>) -> Message {
    use nucleos::contract::{Content, Part, TextPart};
    Message {
        role,
        content: Content::new([Part::new(PartKind::Text(TextPart { text: text.into() }))]),
    }
}
