//! Decoding the Responses API's event stream into the kernel's deltas.
//!
//! Every shape here was taken from a live session, not from a document: the event names, the fact
//! that a tool call arrives as an `output_item.added` whose item carries `call_id` (not `id`), and
//! that usage lives inside `response.completed` and nowhere else.
//!
//! This module is the part worth testing, and it is deliberately free of I/O.

use nucleos::adapter::{FinishReason, ModelDelta};
use nucleos::contract::Usage;
use nucleos::error::{Error, Result};
use serde_json::Value;
use std::collections::BTreeSet;

/// Holds what one event cannot say on its own.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Output indices whose tool call has already been announced.
    started: BTreeSet<u32>,
    finished: Option<FinishReason>,
}

impl Decoder {
    /// Decode one `data:` payload.
    pub fn push(&mut self, payload: &str) -> Result<Vec<ModelDelta>> {
        let trimmed = payload.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(trimmed)
            .map_err(|error| Error::unavailable(format!("cannot decode an event: {error}")))?;
        self.decode(&event)
    }

    /// Whether the stream has said why it stopped.
    #[must_use]
    pub fn finish(&self) -> Option<&FinishReason> {
        self.finished.as_ref()
    }

    fn decode(&mut self, event: &Value) -> Result<Vec<ModelDelta>> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let index = u32::try_from(index).unwrap_or(u32::MAX);

        match kind {
            // A tool call is announced as an item, not as a delta: the `call_id` and the name
            // arrive once, and every argument fragment afterwards refers to it by output index.
            "response.output_item.added" => {
                let Some(item) = event.get("item") else {
                    return Ok(Vec::new());
                };
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(Vec::new());
                }
                if !self.started.insert(index) {
                    return Ok(Vec::new());
                }
                Ok(vec![ModelDelta::ToolCallStarted {
                    index,
                    // `call_id` is what a `function_call_output` must reference. The item also has
                    // an `id`, which is a different thing and useless to the caller.
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }])
            }
            "response.function_call_arguments.delta" => {
                let fragment = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(vec![ModelDelta::ToolCallArgs {
                    index,
                    fragment: fragment.to_owned(),
                }])
            }
            "response.output_text.delta" => {
                let text = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![ModelDelta::Text {
                    part_index: index,
                    text: text.to_owned(),
                }])
            }
            // A reasoning summary is the only reasoning text the API streams; the trace itself
            // stays on the provider unless it is asked for, and its signature has no channel here.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let text = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![ModelDelta::Reasoning {
                    part_index: index,
                    text: text.to_owned(),
                    signature: String::new(),
                }])
            }
            "response.completed" => {
                let response = event.get("response");
                let mut deltas = Vec::new();
                if let Some(usage) = response.and_then(|response| response.get("usage"))
                    && let Some(usage) = decode_usage(usage)
                {
                    deltas.push(ModelDelta::Usage(usage));
                }
                let reason = if response.is_some_and(has_function_call) {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                deltas.push(self.finish_with(reason));
                Ok(deltas)
            }
            "response.incomplete" => {
                let reason = event
                    .get("response")
                    .and_then(|response| response.get("incomplete_details"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str);
                let reason = match reason {
                    Some("max_output_tokens") => FinishReason::Length,
                    Some("content_filter") => FinishReason::Filtered,
                    Some(other) => FinishReason::Other(other.to_owned()),
                    None => FinishReason::Length,
                };
                Ok(vec![self.finish_with(reason)])
            }
            // Both of these arrive inside an HTTP 200: the stream started, and then refused.
            // They still have to land in the right category, or a caller that should have fixed
            // its request retries it instead.
            "response.failed" => Err(stream_error(
                event
                    .get("response")
                    .and_then(|response| response.get("error")),
            )),
            "error" => Err(stream_error(event.get("error"))),
            // `created`, `in_progress`, `content_part.*`, `output_text.done`, … say nothing the
            // kernel acts on.
            _ => Ok(Vec::new()),
        }
    }

    fn finish_with(&mut self, reason: FinishReason) -> ModelDelta {
        self.finished = Some(reason.clone());
        ModelDelta::Finished { reason }
    }
}

/// A failure that arrived inside the stream.
///
/// The shape is `{"error": {"type", "code", "message", "param"}}`. The message is nested, and the
/// category lives in `type` — a fact worth the trouble, because "invalid_request_error" on an HTTP
/// 200 is still the caller's mistake and not a provider outage.
fn stream_error(raw: Option<&Value>) -> Error {
    let raw = raw.cloned().unwrap_or(Value::Null);
    let message = raw
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("the provider failed the response without saying why");

    let kind = raw.get("type").and_then(Value::as_str).unwrap_or_default();
    let error = match kind {
        "invalid_request_error" => Error::invalid(message.to_owned()),
        "authentication_error" | "permission_error" => Error::denied(message.to_owned()),
        "not_found_error" => Error::not_found(message.to_owned()),
        "rate_limit_error" | "insufficient_quota" => Error::budget(message.to_owned()),
        "server_error" | "overloaded_error" => Error::unavailable(message.to_owned()),
        _ => Error::unavailable(message.to_owned()),
    };
    let error = if kind.is_empty() {
        error
    } else {
        error.detail("provider_type", kind)
    };
    match raw.get("param").and_then(Value::as_str) {
        Some(param) => error.detail("param", param),
        None => error,
    }
}

fn has_function_call(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        })
}

fn decode_usage(usage: &Value) -> Option<Usage> {
    let input = usage.get("input_tokens").and_then(Value::as_u64);
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        // The API reports tokens, never money. Guessing a price here would make the ledger's
        // "an unreported counter stays unreported" rule a lie one layer up.
        micros_usd: None,
    })
}
