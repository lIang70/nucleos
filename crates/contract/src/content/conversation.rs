use super::*;

/// Check that a conversation is **well formed** before it is forwarded.
///
/// Two rules, both about structure rather than policy:
///
/// 1. every message passes [`Message::validate`];
/// 2. every tool call is answered — an `assistant` message carrying `ToolCall`
///    parts must be followed by `Tool` messages whose results name those ids, and a
///    result may not name a call that was never issued.
///
/// A provider answers a violation with a 400 that says "invalid request"; naming the
/// message index and the call id is the difference between a fix and a hunt.
pub fn validate_well_formed(messages: &[Message]) -> crate::error::Result<()> {
    let mut pending: Vec<&str> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        message.validate()?;
        match message.role {
            Role::Assistant => {
                // A new assistant turn cannot begin while calls are unanswered.
                if !pending.is_empty() && !message.content.tool_calls().is_empty() {
                    return Err(crate::error::Error::invalid(format!(
                        "message {index} asks for tools while {} earlier call(s) are still unanswered",
                        pending.len()
                    )));
                }
                for call in message.content.tool_calls() {
                    pending.push(&call.id);
                }
            }
            Role::Tool => {
                for part in &message.content.0 {
                    if let Part::ToolResult { result } = part {
                        match pending.iter().position(|id| *id == result.call_id) {
                            Some(position) => {
                                pending.remove(position);
                            }
                            None => {
                                return Err(crate::error::Error::invalid(format!(
                                    "message {index} answers call {:?}, which was never issued",
                                    result.call_id
                                )));
                            }
                        }
                    }
                }
            }
            Role::System | Role::User => {
                if !pending.is_empty() {
                    return Err(crate::error::Error::invalid(format!(
                        "message {index} continues the conversation with {} call(s) unanswered",
                        pending.len()
                    )));
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(crate::error::Error::invalid(format!(
            "{} tool call(s) are never answered: {:?}",
            pending.len(),
            pending
        )));
    }
    Ok(())
}

/// Check that every tool the conversation names is one this session may call.
///
/// Separate from [`validate_well_formed`] because the two fail for different
/// reasons and cost different things: structure is a client bug, a name is a
/// deployment question. The cost of this one is worth stating: a conversation
/// imported from elsewhere names tools this deployment may not have, and it will be
/// refused rather than silently forwarded to a model that cannot call them.
pub fn validate_callable(
    messages: &[Message],
    callable: impl Fn(&str) -> bool,
) -> crate::error::Result<()> {
    for (index, message) in messages.iter().enumerate() {
        for call in message.content.tool_calls() {
            if !callable(call.name.as_str()) {
                return Err(crate::error::Error::invalid(format!(
                    "message {index} calls tool {:?}, which this session cannot use",
                    call.name
                )));
            }
        }
    }
    Ok(())
}
