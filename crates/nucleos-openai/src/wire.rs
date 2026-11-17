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
use nucleos::contract::{Degraded, Field, Message, Output, PartKind, Role, ToolSpec};
use nucleos::error::{Error, Result};
use serde_json::{Value, json};

// The parameters that belong to a deployment are the kernel's `Hints`, not a second vocabulary:
// one set of enums, one set of names, and one place to add the next one.
pub use nucleos::contract::{Hints, ServiceTier, Summary, Truncation, Verbosity};

/// Which of the provider's endpoints this call goes to.
///
/// OpenAI serves the three output modalities from three URLs, and they do not share a body shape:
/// text is a conversation on `/responses`, an image is a prompt on `/images/generations`, speech is
/// text on `/audio/speech`. `Output` decides which one, which is the whole point of having one output
/// axis — the *caller* picks a modality, and the adapter knows where it belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The Responses API: one conversation, one answer.
    Responses,
    /// The image generation API: one prompt, one image.
    Images,
    /// The speech synthesis API: one text, one clip.
    Speech,
}

/// A request body, and what had to be left out of it.
#[derive(Debug, Clone)]
pub struct Encoded {
    /// The JSON body.
    pub body: Value,
    /// Fields the provider has no channel for.
    pub dropped: Vec<Degraded>,
    /// Where the body is sent.
    pub endpoint: Endpoint,
    /// The media type the answer's payload will have, when the surface knows before the call.
    ///
    /// A generated image's type comes from the format the caller asked for, so reading it here rather
    /// than from the response means a payload cannot be labelled with a type nobody requested.
    pub answer_media_type: Option<String>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Include {
    /// The encrypted reasoning payload, so a reasoning turn can be replayed statelessly.
    ReasoningEncryptedContent,
}

/// Build the body for whichever endpoint this request's output belongs to.
///
/// One entry point rather than three, because the caller chose a modality and the adapter is the
/// only thing that knows where that modality lives. `extensions` still arrive on the request; a
/// surface consumes the ones addressed to it and this function accounts for the rest.
///
/// # Errors
///
/// An output this adapter does not serve, or a request the chosen endpoint cannot express — an image
/// call with no prompt text, for instance.
pub fn encode(request: &ModelRequest, settings: Settings<'_>) -> Result<Encoded> {
    match request.output {
        Output::Text => Ok(encode_responses(request, settings)),
        Output::Image => encode_image(request, settings),
        Output::Audio => encode_speech(request, settings),
        Output::Video => Err(Error::new(
            nucleos::contract::ErrorCode::UnsupportedFeature,
            "this adapter cannot generate video: no endpoint here serves `Output::Video`",
        )),
    }
}

/// Build a Responses body from a kernel request.
fn encode_responses(
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
    for message in request.messages.iter() {
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

    Encoded {
        body,
        dropped,
        endpoint: Endpoint::Responses,
        answer_media_type: None,
    }
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
    match &params.tool_choice {
        ToolChoice::Auto => body["tool_choice"] = json!("auto"),
        ToolChoice::Required => body["tool_choice"] = json!("required"),
        ToolChoice::None => body["tool_choice"] = json!("none"),
        // A forced call names the function in the choice itself: the Responses API takes an object
        // here, not a bare string.
        ToolChoice::Function { name } => {
            body["tool_choice"] = json!({ "type": "function", "name": name });
        }
    }

    if !params.stop.is_empty() {
        // Verified against the live endpoint: `Unknown parameter: 'stop'`. Dropping it silently
        // would leave a caller wondering why their text kept running past the sequence.
        dropped.push(Degraded {
            field: nucleos::contract::Field::Param(nucleos::contract::Param::Stop),
            reason: "the Responses API has no stop parameter".to_owned(),
            code: Some("not_supported".to_owned()),
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
                field: nucleos::contract::Field::Param(nucleos::contract::Param::ReasoningEffort),
                reason: "this model does not support reasoning effort".to_owned(),
                code: Some("not_supported".to_owned()),
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
                field: nucleos::contract::Field::Messages,
                reason: "history reasoning has no channel back to the provider".to_owned(),
                code: Some("implicit_off".to_owned()),
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
            // This dialect has no channel for audio or video in history. Reported rather than
            // dropped in silence: a caller that replayed a conversation containing a clip needs to
            // know the model never saw it.
            PartKind::Audio(_) | PartKind::Video(_) => dropped.push(Degraded {
                field: nucleos::contract::Field::Messages,
                reason: "this endpoint has no channel for audio or video in history".to_owned(),
                code: Some("implicit_off".to_owned()),
            }),
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

fn image_url(source: &nucleos::contract::ImageSource) -> String {
    match source.from() {
        nucleos::contract::MediaFrom::Url { url } => url.clone(),
        nucleos::contract::MediaFrom::Data { data } => {
            format!(
                "data:{};base64,{}",
                source.media_type(),
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

// ---------------------------------------------------------------------------
// The image surface
// ---------------------------------------------------------------------------

/// Build an image generation body.
///
/// The images endpoint takes a prompt and settings, not a conversation: there is nowhere in its body
/// for a system message, a tool, a sampling knob or a hint. Every field the caller did set that this
/// endpoint has no place for is therefore *reported* rather than silently absent — the contract check
/// requires an adapter to honour or explain each one, and a narrower surface has more to explain.
fn encode_image(request: &ModelRequest, settings: Settings<'_>) -> Result<Encoded> {
    let options = image_options(request);
    let prompt = image_prompt(request)?;

    let mut body = json!({
        "model": settings.model,
        "prompt": prompt,
        // Always streamed: previews are the reason a caller can render something before the image is
        // finished, and a unary call has no way to deliver them.
        "stream": true,
    });
    if let Some(options) = options {
        if let Some(count) = options.count {
            body["n"] = json!(count);
        }
        if let Some(size) = &options.size {
            body["size"] = json!(size);
        }
        if let Some(quality) = &options.quality {
            body["quality"] = json!(quality);
        }
        if let Some(style) = &options.style {
            body["style"] = json!(style);
        }
        if let Some(format) = &options.output_format {
            body["output_format"] = json!(format);
        }
        if let Some(format) = &options.response_format {
            body["response_format"] = json!(format);
        }
        if let Some(background) = &options.background {
            body["background"] = json!(background);
        }
        if let Some(moderation) = &options.moderation {
            body["moderation"] = json!(moderation);
        }
        if let Some(compression) = options.output_compression {
            body["output_compression"] = json!(compression);
        }
        if let Some(partial) = options.partial_images {
            body["partial_images"] = json!(partial);
        }
    }

    let mut honoured = vec![Field::Messages, Field::Output];
    honoured.extend(own_image_fields(request));
    let dropped = unhonoured(request, &honoured, "image");

    Ok(Encoded {
        body,
        dropped,
        endpoint: Endpoint::Images,
        answer_media_type: Some(image_media_type(options)),
    })
}

/// The image options on this request, when the deployment set any.
pub fn image_options(request: &ModelRequest) -> Option<&crate::extension::ImageOptions> {
    request
        .extensions
        .for_provider("openai")
        .find_map(|extension| {
            extension
                .as_any()
                .downcast_ref::<crate::extension::ImageOptions>()
        })
}

/// The qualified identities of this adapter's own image options.
fn own_image_fields(request: &ModelRequest) -> Vec<Field> {
    request
        .extensions
        .active_fields()
        .into_iter()
        .filter(|field| {
            matches!(
                field,
                Field::Extension { provider, id, .. }
                    if provider == "openai" && id == crate::extension::IMAGE_EXTENSION_ID
            )
        })
        .collect()
}

/// The prompt of an image call: the text the caller wrote, joined in order.
///
/// Not "the last message": an image prompt assembled from several turns is still one prompt, and
/// silently keeping only the newest would drop what the caller said.
fn image_prompt(request: &ModelRequest) -> Result<String> {
    let mut prompt = String::new();
    for message in request.messages.iter() {
        let text = message.content.text();
        if text.trim().is_empty() {
            continue;
        }
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&text);
    }
    if prompt.trim().is_empty() {
        return Err(Error::invalid(
            "an image call needs a prompt, and this request carries no text to use as one",
        ));
    }
    Ok(prompt)
}

/// The media type an image payload will have, from the format the caller asked for.
#[must_use]
pub fn image_media_type(options: Option<&crate::extension::ImageOptions>) -> String {
    match options.and_then(|options| options.output_format.as_deref()) {
        Some("jpeg") => "image/jpeg".to_owned(),
        Some("webp") => "image/webp".to_owned(),
        _ => "image/png".to_owned(),
    }
}

/// Every field the request set that this surface has no channel for.
fn unhonoured(request: &ModelRequest, honoured: &[Field], surface: &str) -> Vec<Degraded> {
    request
        .active_fields()
        .into_iter()
        .filter(|field| !honoured.contains(field))
        .map(|field| Degraded {
            field: field.clone(),
            reason: format!(
                "the {surface} endpoint has no place for `{}`: it takes a prompt and nothing else",
                field.as_str()
            ),
            code: Some("not_supported".to_owned()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The speech surface
// ---------------------------------------------------------------------------

/// Build a speech synthesis body.
///
/// The endpoint takes the text to read and the voice to read it in: no conversation, no tools, no
/// sampling. As with images, every field this body has no place for is reported rather than dropped —
/// and `stream_format: audio` is what makes chunked delivery possible, so the answer is a byte stream
/// rather than frames.
fn encode_speech(request: &ModelRequest, settings: Settings<'_>) -> Result<Encoded> {
    let options = speech_options(request);
    let text = speech_text(request)?;

    let mut body = json!({
        "model": settings.model,
        "input": text,
        "stream_format": "audio",
    });
    if let Some(options) = options {
        if let Some(voice) = &options.voice {
            body["voice"] = json!(voice);
        }
        if let Some(format) = &options.format {
            body["response_format"] = json!(format);
        }
        if let Some(speed) = options.speed {
            body["speed"] = json!(speed);
        }
        if let Some(instructions) = &options.instructions {
            body["instructions"] = json!(instructions);
        }
    }

    let mut honoured = vec![Field::Messages, Field::Output];
    honoured.extend(own_speech_fields(request));
    let dropped = unhonoured(request, &honoured, "speech");

    Ok(Encoded {
        body,
        dropped,
        endpoint: Endpoint::Speech,
        answer_media_type: Some(speech_media_type(options)),
    })
}

/// The speech options on this request, when the deployment set any.
pub fn speech_options(request: &ModelRequest) -> Option<&crate::extension::SpeechOptions> {
    request
        .extensions
        .for_provider("openai")
        .find_map(|extension| {
            extension
                .as_any()
                .downcast_ref::<crate::extension::SpeechOptions>()
        })
}

/// The qualified identities of this adapter's own speech options.
fn own_speech_fields(request: &ModelRequest) -> Vec<Field> {
    request
        .extensions
        .active_fields()
        .into_iter()
        .filter(|field| {
            matches!(
                field,
                Field::Extension { provider, id, .. }
                    if provider == "openai" && id == crate::extension::SPEECH_EXTENSION_ID
            )
        })
        .collect()
}

/// The text to read: everything the caller wrote, in order.
fn speech_text(request: &ModelRequest) -> Result<String> {
    let mut text = String::new();
    for message in request.messages.iter() {
        let part = message.content.text();
        if part.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&part);
    }
    if text.trim().is_empty() {
        return Err(Error::invalid(
            "a speech call needs text to read, and this request carries none",
        ));
    }
    Ok(text)
}

/// The media type of the clip, from the format the caller asked for.
///
/// The provider's default is mp3, so an absent format still has a type — one that must match what the
/// bytes actually are, or the two chunks of a stream would not merge and the part would be a lie.
#[must_use]
pub fn speech_media_type(options: Option<&crate::extension::SpeechOptions>) -> String {
    match options.and_then(|options| options.format.as_deref()) {
        Some("opus") => "audio/opus".to_owned(),
        Some("aac") => "audio/aac".to_owned(),
        Some("flac") => "audio/flac".to_owned(),
        Some("wav") => "audio/wav".to_owned(),
        Some("pcm") => "audio/pcm".to_owned(),
        _ => "audio/mpeg".to_owned(),
    }
}
