//! The Responses API's dialect: request bodies, and the framing of its stream.
//!
//! The shapes here were verified against the live endpoint. Three of them are worth naming because
//! they are not what a chat-completions habit would produce:
//!
//! - **A tool is flat.** `{type, name, parameters}` at the top level, not nested under `function`.
//! - **A tool call in history is an input item**, `{type: "function_call", call_id, …}`, and its
//!   result is `{type: "function_call_output", call_id, output}` — not a message with a role.
//! - **`store` is off, always.** The API can keep the conversation for us; the kernel's premise is
//!   that its own event log is the record. A provider holding half the state would make replay a
//!   reconstruction of something that no longer exists.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nucleos::adapter::{GenerateParams, ModelRequest, ReasoningEffort, TextFormat, ToolChoice};
use nucleos::contract::{Degraded, Message, PartKind, Role, ToolSpec};
use serde_json::{Value, json};

// The parameters that belong to a deployment are the kernel's `Hints`, not a second vocabulary:
// one set of enums, one set of names, and one place to add the next one.
pub use nucleos::contract::{Hints, ServiceTier, Summary, Truncation, Verbosity};

/// A request body, and what had to be left out of it.
#[derive(Debug, Clone)]
pub struct Encoded {
    /// The JSON body.
    pub body: Value,
    /// Fields the provider has no channel for.
    pub dropped: Vec<Degraded>,
}

/// Everything about a deployment that shapes the request, as opposed to about a call.
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// The model name to send.
    pub model: &'a str,
    /// Whether the model accepts `reasoning.effort`.
    pub supports_reasoning: bool,
    /// Whether tool schemas are declared strict.
    ///
    /// A promise the *schema* has to be able to keep: the provider accepts `strict: true` only for
    /// a schema with `additionalProperties: false` and every property required. Forcing it on a
    /// schema that is not eligible turns every call offering the tool into a 400 — measured, not
    /// guessed — so it is configuration.
    pub strict_tools: bool,
    /// The deployment's defaults. The call's hints override them field by field.
    pub options: &'a Hints,
    /// The provider's prefix-cache key. The host derives it, usually from the session.
    pub cache_key: Option<&'a str>,
    /// Extra data to ask the provider to include.
    pub include: &'a [Include],
    /// A privacy-preserving identifier for the end user. The host derives it from the principal.
    pub safety_identifier: Option<&'a str>,
}

/// Extra data to ask the provider to include in the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Include {
    /// The encrypted reasoning payload, so a reasoning turn can be replayed statelessly.
    ReasoningEncryptedContent,
}

/// Build a Responses body from a kernel request.
#[must_use]
pub fn encode(
    request: &ModelRequest,
    Settings {
        model,
        supports_reasoning,
        strict_tools,
        options,
        cache_key,
        include,
        safety_identifier,
    }: Settings<'_>,
) -> Encoded {
    let mut dropped = Vec::new();
    let mut input = Vec::new();

    // A system message and `instructions` are the same channel here; the request's `system` wins
    // by being written second, because that is the one the kernel put there deliberately.
    let mut instructions = String::new();
    for message in &request.messages {
        encode_message(message, &mut input, &mut dropped);
    }
    if let Some(system) = &request.system {
        instructions.push_str(system);
    }

    let mut body = json!({
        "model": model,
        "input": input,
        // Not negotiable: see the module comment.
        "store": false,
        "stream": true,
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| encode_tool(tool, strict_tools))
                .collect(),
        );
    }
    encode_params(&request.params, supports_reasoning, &mut body, &mut dropped);
    // The call's hints win over the deployment's defaults, field by field: a session that asked for
    // terse answers while it fills its window should not be overruled by a config file.
    encode_options(&options.merged(&request.hints), &mut body);
    if let Some(key) = cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    if let Some(identifier) = safety_identifier {
        body["safety_identifier"] = json!(identifier);
    }
    if !include.is_empty() {
        body["include"] = json!(
            include
                .iter()
                .map(|include| match include {
                    Include::ReasoningEncryptedContent => "reasoning.encrypted_content",
                })
                .collect::<Vec<_>>()
        );
    }

    Encoded { body, dropped }
}

fn encode_params(
    params: &GenerateParams,
    supports_reasoning: bool,
    body: &mut Value,
    dropped: &mut Vec<Degraded>,
) {
    if let Some(max) = params.max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(temperature) = params.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = params.top_p {
        body["top_p"] = json!(top_p);
    }
    if let Some(parallel) = params.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(parallel);
    }
    match params.tool_choice {
        ToolChoice::Auto => body["tool_choice"] = json!("auto"),
        ToolChoice::Required => body["tool_choice"] = json!("required"),
        ToolChoice::None => body["tool_choice"] = json!("none"),
    }

    if !params.stop.is_empty() {
        // Verified against the live endpoint: `Unknown parameter: 'stop'`. Dropping it silently
        // would leave a caller wondering why their text kept running past the sequence.
        dropped.push(Degraded {
            field: "params.stop".to_owned(),
            reason: "the Responses API has no stop parameter".to_owned(),
        });
    }

    match params.reasoning_effort {
        Some(effort) if supports_reasoning => {
            body["reasoning"] = json!({
                "effort": match effort {
                    ReasoningEffort::Minimal => "minimal",
                    ReasoningEffort::Low => "low",
                    ReasoningEffort::Medium => "medium",
                    ReasoningEffort::High => "high",
                },
            });
        }
        Some(_) => {
            // Not every model accepts it, and a 400 would kill a call over a hint. The adapter
            // knows from its configuration, and says so instead of guessing or failing.
            dropped.push(Degraded {
                field: "params.reasoning_effort".to_owned(),
                reason: "this model does not support reasoning effort".to_owned(),
            });
        }
        None => {}
    }

    match &params.format {
        TextFormat::Text => {}
        TextFormat::JsonObject => {
            body["text"] = json!({ "format": { "type": "json_object" } });
        }
        TextFormat::JsonSchema { name, schema_json } => {
            let schema = serde_json::from_str::<Value>(schema_json).unwrap_or_else(|_| json!({}));
            body["text"] = json!({
                "format": {
                    "type": "json_schema",
                    "name": name,
                    "schema": schema,
                    "strict": true,
                },
            });
        }
    }
}

/// Merge the deployment's parameters into a body already built from the call's.
///
/// `text` and `reasoning` are assembled from both sides, so this merges rather than overwrites:
/// a call that asked for a JSON schema and a deployment that asked for verbosity should get both.
fn encode_options(options: &Hints, body: &mut Value) {
    if !options.metadata.is_empty() {
        body["metadata"] = json!(options.metadata);
    }
    if let Some(tier) = options.service_tier {
        body["service_tier"] = json!(match tier {
            ServiceTier::Auto => "auto",
            ServiceTier::Default => "default",
            ServiceTier::Flex => "flex",
            ServiceTier::Priority => "priority",
        });
    }
    if let Some(truncation) = options.truncation {
        body["truncation"] = json!(match truncation {
            Truncation::Auto => "auto",
            Truncation::Disabled => "disabled",
        });
    }
    if let Some(max) = options.max_tool_calls {
        body["max_tool_calls"] = json!(max);
    }
    if let Some(verbosity) = options.verbosity {
        body["text"]["verbosity"] = json!(match verbosity {
            Verbosity::Low => "low",
            Verbosity::Medium => "medium",
            Verbosity::High => "high",
        });
    }
    if let Some(summary) = options.reasoning_summary {
        body["reasoning"]["summary"] = json!(match summary {
            Summary::Auto => "auto",
            Summary::Concise => "concise",
            Summary::Detailed => "detailed",
        });
    }
}

fn encode_tool(tool: &ToolSpec, strict: bool) -> Value {
    let schema =
        serde_json::from_str::<Value>(&tool.input_schema_json).unwrap_or_else(|_| json!({}));
    // Flat: the Responses API puts `name` and `parameters` on the tool, not under `function`.
    let mut encoded = json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": schema,
    });
    if strict {
        encoded["strict"] = json!(true);
    }
    encoded
}

fn encode_message(message: &Message, out: &mut Vec<Value>, dropped: &mut Vec<Degraded>) {
    let mut text = String::new();
    let mut images = Vec::new();

    for part in &message.content.parts {
        match &part.kind {
            PartKind::Text(part) => push_text(&mut text, &part.text),
            PartKind::Reasoning(_) => dropped.push(Degraded {
                field: "message.reasoning".to_owned(),
                reason: "history reasoning has no channel back to the provider".to_owned(),
            }),
            PartKind::ToolCall(part) => out.push(json!({
                "type": "function_call",
                "call_id": part.call.id,
                "name": part.call.name,
                "arguments": part.call.arguments_json,
            })),
            PartKind::ToolResult(part) => out.push(json!({
                "type": "function_call_output",
                "call_id": part.result.call_id,
                "output": part.result.content.text(),
            })),
            PartKind::Image(image) => images.push(json!({
                "type": "input_image",
                "image_url": image_url(&image.source),
            })),
            PartKind::File(file) => push_text(
                &mut text,
                &format!("[file: {} ({})]", file.uri, file.media_type),
            ),
            PartKind::Data(data) => push_text(&mut text, &data.json),
        }
    }

    // Tool items are already their own entries; a role message is only for text and images.
    if text.is_empty() && images.is_empty() {
        return;
    }
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    };

    let content = if images.is_empty() {
        let kind = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        json!([{ "type": kind, "text": text }])
    } else {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({ "type": "input_text", "text": text }));
        }
        parts.extend(images);
        json!(parts)
    };

    out.push(json!({ "type": "message", "role": role, "content": content }));
}

fn push_text(into: &mut String, text: &str) {
    if !into.is_empty() {
        into.push('\n');
    }
    into.push_str(text);
}

fn image_url(source: &nucleos::contract::MediaSource) -> String {
    match &source.from {
        nucleos::contract::MediaFrom::Url { url } => url.clone(),
        nucleos::contract::MediaFrom::Data { data } => {
            format!(
                "data:{};base64,{}",
                source.media_type,
                STANDARD.encode(data)
            )
        }
    }
}

/// Reassembles server-sent events out of a byte stream.
///
/// A stream is not a sequence of messages: it is a sequence of bytes that occasionally contains
/// one. This keeps the remainder between reads, because a chunk boundary falls wherever the
/// network decided it should.
#[derive(Debug, Default)]
pub struct Framer {
    buffer: String,
    pending: String,
}

impl Framer {
    /// Feed bytes, get whatever complete payloads they finished.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut payloads = Vec::new();

        while let Some(newline) = self.buffer.find('\n') {
            let line = self.buffer[..newline].trim_end_matches('\r').to_owned();
            self.buffer.drain(..=newline);

            if line.is_empty() {
                if !self.pending.is_empty() {
                    payloads.push(std::mem::take(&mut self.pending));
                }
                continue;
            }
            // `event:` names what the payload's own `type` also names, and `id:` is the provider's
            // cursor, which the kernel does not use: its own `seq` is the cursor.
            if let Some(data) = line.strip_prefix("data:") {
                if !self.pending.is_empty() {
                    self.pending.push('\n');
                }
                self.pending.push_str(data.trim_start());
            }
        }
        payloads
    }
}
