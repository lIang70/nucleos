//! The reference agent loop, built on the kernel's syscalls.
//!
//! This is a driver, not a kernel. Everything it does is something an application could do
//! itself: open a session, ask for an inference, run the tools it asked for, append the results,
//! repeat until a stop condition fires, close.
//!
//! It exists because the loop's subtle rules are the part nobody should have to rediscover —
//! an attempt that produced content is never retried, a batch asks policy before it runs, broken
//! arguments never reach a tool, and stopping is four layers deep. An application that wants a
//! different loop should write one; an application that wants *this* loop should not write it
//! again.

use nucleos::adapter::FinishReason;
use nucleos::contract::Principal;
use nucleos::contract::{
    EventKind, Message, PartKind, Role, SessionId, SessionState, StopReason, ToolCall,
};
use nucleos::engine::{HitlVerdict, Kernel, Outcome, Session, SessionSpec, StateWrite};
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
    if let Some(reason) = session.exhausted(kernel.now()) {
        kernel
            .record(session, EventKind::BudgetExhausted { reason })
            .await?;
        kernel.close(session, reason).await?;
        return Ok(Step::Stopped(reason));
    }
    if session.is_closed() {
        return Ok(Step::Stopped(StopReason::Completed));
    }

    let outstanding = outstanding_calls(session);
    if outstanding.is_empty() {
        return inference_turn(kernel, session).await;
    }
    tool_turn(kernel, session, &outstanding).await
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
    let inference = match kernel.inference(session, &messages).await {
        Ok(inference) => inference,
        Err(error) => {
            kernel
                .record(
                    session,
                    EventKind::Failed {
                        code: error.code(),
                        reason: error.reason().to_owned(),
                    },
                )
                .await?;
            kernel.close(session, StopReason::Failed).await?;
            return Ok(Step::Stopped(StopReason::Failed));
        }
    };

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
    let events = kernel.events(session.id(), 0).await?;
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
