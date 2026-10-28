//! Kernel types ↔ wire types.
//!
//! Every value that exists only because the wire needs it stops here. The kernel never sees a
//! `pb::` type, and this module is the only place that knows both vocabularies — which is what
//! makes "the kernel must not learn the wire" a property of the code and not a promise in a
//! document.

use nucleos::contract as k;
use nucleos_proto::pb;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A kernel event, as the wire carries it.
pub fn event(event: &k::Event) -> pb::Event {
    pb::Event {
        seq: event.seq,
        session: event.session.as_str().to_owned(),
        at: Some(timestamp(event.at_unix_millis)),
        schema_version: event.schema_version,
        // Redundant with `kind` on purpose: a proxy filters on it without compiling the schema.
        r#type: event.kind.type_name().to_owned(),
        meta: event
            .meta
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        kind: Some(event_kind(&event.kind)),
    }
}

fn event_kind(kind: &k::EventKind) -> pb::event::Kind {
    use pb::event::Kind;
    match kind {
        k::EventKind::StateChanged { state, stop } => Kind::State(pb::StateChanged {
            state: state_to_i32(*state),
            stop: stop.map_or(0, stop_to_i32),
        }),
        k::EventKind::TextDelta { part_index, text } => Kind::Text(pb::TextDelta {
            part_index: *part_index,
            text: text.clone(),
        }),
        k::EventKind::ReasoningDelta {
            part_index,
            text,
            signature,
        } => Kind::Reasoning(pb::ReasoningDelta {
            part_index: *part_index,
            text: text.clone(),
            signature: signature.clone(),
        }),
        k::EventKind::ToolCallStarted {
            index,
            tool_call_id,
            name,
            deadline_unix_millis,
            progress_token,
        } => Kind::ToolCall(pb::ToolCallStarted {
            index: *index,
            tool_call_id: tool_call_id.clone(),
            name: name.clone(),
            deadline_unix_millis: i64::try_from(*deadline_unix_millis).unwrap_or(i64::MAX),
            progress_token: progress_token.clone(),
        }),
        k::EventKind::ToolCallArgs { index, fragment } => Kind::ToolCallArgs(pb::ToolCallArgs {
            index: *index,
            fragment: fragment.clone(),
        }),
        k::EventKind::ToolFinished {
            duration_millis,
            result,
        } => Kind::ToolResult(pb::ToolFinished {
            duration_millis: *duration_millis,
            result: Some(result_to_pb(result)),
        }),
        k::EventKind::Usage {
            usage,
            purpose,
            model,
            attempt_index,
        } => Kind::Usage(pb::UsageReport {
            usage: Some(usage_to_pb(usage)),
            purpose: purpose_to_i32(*purpose),
            model: model.clone(),
            attempt_index: *attempt_index,
        }),
        k::EventKind::AwaitingHuman {
            question,
            tool_call_id,
            options,
            default_if_unanswered,
        } => Kind::AwaitingHuman(pb::AwaitingHuman {
            question: question.clone(),
            tool_call_id: tool_call_id.clone(),
            options: options.iter().map(option_to_pb).collect(),
            default_if_unanswered: permission_to_i32(*default_if_unanswered),
        }),
        k::EventKind::BudgetExhausted { reason } => Kind::BudgetExhausted(pb::BudgetExhausted {
            reason: stop_to_i32(*reason),
        }),
        k::EventKind::Failed { code, reason } => Kind::Failed(pb::Failure {
            code: code_to_i32(*code),
            reason: reason.clone(),
        }),
        k::EventKind::GenerationMigrated { from, to } => {
            Kind::GenerationMigrated(pb::GenerationMigrated {
                from_generation: from.0,
                to_generation: to.0,
            })
        }
        k::EventKind::MessageCompleted {
            message,
            step_index,
        } => Kind::MessageCompleted(pb::MessageCompleted {
            message: Some(message_to_pb(message)),
            step_index: *step_index,
        }),
        k::EventKind::CapabilityDegraded(degraded) => Kind::Degraded(pb::CapabilityDegraded {
            field: degraded.field.clone(),
            reason: degraded.reason.clone(),
        }),
        k::EventKind::AttemptFailed { attempt } => Kind::AttemptFailed(pb::AttemptFailed {
            attempt: Some(attempt_to_pb(attempt)),
        }),
        k::EventKind::Progress {
            token,
            done,
            total,
            message,
        } => Kind::Progress(pb::Progress {
            token: token.clone(),
            done: *done,
            total: *total,
            message: message.clone(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

/// A message, as the wire carries it.
pub fn message_to_pb(message: &k::Message) -> pb::Message {
    pb::Message {
        role: role_to_i32(message.role),
        content: Some(content_to_pb(&message.content)),
    }
}

/// A message, from the wire.
pub fn message_from_pb(message: &pb::Message) -> k::Message {
    k::Message {
        role: role_from_i32(message.role),
        content: message
            .content
            .as_ref()
            .map_or_else(k::Content::default, content_from_pb),
    }
}

fn content_to_pb(content: &k::Content) -> pb::Content {
    pb::Content {
        parts: content.parts.iter().map(part_to_pb).collect(),
    }
}

fn content_from_pb(content: &pb::Content) -> k::Content {
    k::Content::new(content.parts.iter().filter_map(part_from_pb))
}

fn part_to_pb(part: &k::Part) -> pb::Part {
    use pb::part::Kind;
    let kind = match &part.kind {
        k::PartKind::Text(text) => Kind::Text(pb::TextPart {
            text: text.text.clone(),
        }),
        k::PartKind::Image(image) => Kind::Image(pb::ImagePart {
            source: Some(media_to_pb(&image.source)),
        }),
        k::PartKind::File(file) => Kind::File(pb::FilePart {
            uri: file.uri.clone(),
            media_type: file.media_type.clone(),
            name: file.name.clone(),
        }),
        k::PartKind::Data(data) => Kind::Data(pb::DataPart {
            media_type: data.media_type.clone(),
            json: data.json.clone(),
        }),
        k::PartKind::ToolCall(call) => Kind::ToolCall(pb::ToolCallPart {
            call: Some(tool_call_to_pb(&call.call)),
        }),
        k::PartKind::ToolResult(result) => Kind::ToolResult(pb::ToolResultPart {
            result: Some(result_to_pb(&result.result)),
        }),
        k::PartKind::Reasoning(reasoning) => Kind::Reasoning(pb::ReasoningPart {
            text: reasoning.text.clone(),
            signature: reasoning.signature.clone(),
            id: reasoning.id.clone(),
            source: reasoning.source.clone(),
        }),
    };
    pb::Part {
        meta: part
            .meta
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        kind: Some(kind),
    }
}

/// `None` for a part kind this build does not know.
///
/// Additive is not the same as understood: a part kind from a newer schema is carried by the
/// SDKs that know it and dropped by this one, rather than failing the whole message.
fn part_from_pb(part: &pb::Part) -> Option<k::Part> {
    use pb::part::Kind;
    let kind = match part.kind.as_ref()? {
        Kind::Text(text) => k::PartKind::Text(k::TextPart {
            text: text.text.clone(),
        }),
        Kind::Image(image) => k::PartKind::Image(k::ImagePart {
            source: image.source.as_ref().map_or_else(
                || k::MediaSource {
                    media_type: String::new(),
                    from: k::MediaFrom::Data { data: Vec::new() },
                },
                media_from_pb,
            ),
        }),
        Kind::File(file) => k::PartKind::File(k::FilePart {
            uri: file.uri.clone(),
            media_type: file.media_type.clone(),
            name: file.name.clone(),
        }),
        Kind::Data(data) => k::PartKind::Data(k::DataPart {
            media_type: data.media_type.clone(),
            json: data.json.clone(),
        }),
        Kind::ToolCall(call) => k::PartKind::ToolCall(k::ToolCallPart {
            call: call
                .call
                .as_ref()
                .map_or_else(empty_call, tool_call_from_pb),
        }),
        Kind::ToolResult(result) => k::PartKind::ToolResult(k::ToolResultPart {
            result: result
                .result
                .as_ref()
                .map_or_else(empty_result, result_from_pb),
        }),
        Kind::Reasoning(reasoning) => k::PartKind::Reasoning(k::ReasoningPart {
            text: reasoning.text.clone(),
            signature: reasoning.signature.clone(),
            id: reasoning.id.clone(),
            source: reasoning.source.clone(),
        }),
    };
    Some(k::Part {
        kind,
        meta: part
            .meta
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
    })
}

fn empty_call() -> k::ToolCall {
    k::ToolCall {
        id: String::new(),
        name: String::new(),
        arguments_json: String::new(),
        meta: BTreeMap::new(),
    }
}

fn empty_result() -> k::ToolResult {
    k::ToolResult {
        call_id: String::new(),
        content: k::Content::default(),
        is_error: true,
    }
}

fn media_to_pb(source: &k::MediaSource) -> pb::MediaSource {
    let from = match &source.from {
        k::MediaFrom::Url { url } => pb::media_source::From::Url(url.clone()),
        k::MediaFrom::Data { data } => pb::media_source::From::Data(data.clone()),
    };
    pb::MediaSource {
        media_type: source.media_type.clone(),
        from: Some(from),
    }
}

fn media_from_pb(source: &pb::MediaSource) -> k::MediaSource {
    use pb::media_source::From;
    let from = match source.from.as_ref() {
        Some(From::Url(url)) => k::MediaFrom::Url { url: url.clone() },
        Some(From::Data(data)) => k::MediaFrom::Data { data: data.clone() },
        None => k::MediaFrom::Data { data: Vec::new() },
    };
    k::MediaSource {
        media_type: source.media_type.clone(),
        from,
    }
}

fn tool_call_to_pb(call: &k::ToolCall) -> pb::ToolCall {
    pb::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
        meta: call
            .meta
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn tool_call_from_pb(call: &pb::ToolCall) -> k::ToolCall {
    k::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
        meta: call
            .meta
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

fn result_to_pb(result: &k::ToolResult) -> pb::ToolResult {
    pb::ToolResult {
        call_id: result.call_id.clone(),
        content: Some(content_to_pb(&result.content)),
        is_error: result.is_error,
    }
}

fn result_from_pb(result: &pb::ToolResult) -> k::ToolResult {
    k::ToolResult {
        call_id: result.call_id.clone(),
        content: result
            .content
            .as_ref()
            .map_or_else(k::Content::default, content_from_pb),
        is_error: result.is_error,
    }
}

// ---------------------------------------------------------------------------
// Accounting and identifiers
// ---------------------------------------------------------------------------

/// Usage, as the wire carries it.
pub fn usage_to_pb(usage: &k::Usage) -> pb::Usage {
    pb::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        micros_usd: usage.micros_usd,
    }
}

fn attempt_to_pb(attempt: &k::Attempt) -> pb::Attempt {
    use k::AttemptOutcome;
    let (error, degraded) = match &attempt.outcome {
        AttemptOutcome::Ok => (None, attempt.degraded.clone()),
        AttemptOutcome::Failed { code, reason } => (
            Some(pb::Error {
                code: code_to_i32(*code),
                reason: reason.clone(),
                retryable: code.is_retryable(),
                retry_after_millis: 0,
                details: Default::default(),
            }),
            attempt.degraded.clone(),
        ),
    };
    pb::Attempt {
        index: attempt.index,
        model: attempt.model.clone(),
        phase: phase_to_i32(attempt.phase),
        trigger: trigger_to_i32(attempt.trigger),
        error,
        usage: Some(usage_to_pb(&attempt.usage)),
        duration_millis: attempt.duration_millis,
        degraded: degraded.iter().map(degraded_to_pb).collect(),
    }
}

fn degraded_to_pb(degraded: &k::Degraded) -> pb::CapabilityDegraded {
    pb::CapabilityDegraded {
        field: degraded.field.clone(),
        reason: degraded.reason.clone(),
    }
}

fn option_to_pb(option: &k::PermissionOption) -> pb::PermissionOption {
    pb::PermissionOption {
        id: option.id.clone(),
        kind: permission_to_i32(option.kind),
        label: option.label.clone(),
    }
}

/// An error, as the wire carries it.
pub fn error_to_pb(error: &nucleos::Error) -> pb::Error {
    pb::Error {
        code: code_to_i32(error.code()),
        reason: error.reason().to_owned(),
        retryable: error.is_retryable(),
        retry_after_millis: error.retry_after_millis(),
        details: error
            .details()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

fn timestamp(unix_millis: u64) -> prost_types::Timestamp {
    let seconds = i64::try_from(unix_millis / 1_000).unwrap_or(i64::MAX);
    let nanos = i32::try_from((unix_millis % 1_000) * 1_000_000).unwrap_or(0);
    prost_types::Timestamp { seconds, nanos }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

fn state_to_i32(state: k::SessionState) -> i32 {
    let value = match state {
        k::SessionState::Created => pb::SessionState::Created,
        k::SessionState::Running => pb::SessionState::Running,
        k::SessionState::AwaitingTool => pb::SessionState::AwaitingTool,
        k::SessionState::AwaitingHuman => pb::SessionState::AwaitingHuman,
        k::SessionState::Cancelling => pb::SessionState::Cancelling,
        k::SessionState::Done => pb::SessionState::Done,
        k::SessionState::Failed => pb::SessionState::Failed,
        k::SessionState::Cancelled => pb::SessionState::Cancelled,
    };
    value as i32
}

fn stop_to_i32(reason: k::StopReason) -> i32 {
    let value = match reason {
        k::StopReason::Completed => pb::StopReason::Completed,
        k::StopReason::Steps => pb::StopReason::Steps,
        k::StopReason::Tokens => pb::StopReason::Tokens,
        k::StopReason::Cost => pb::StopReason::Cost,
        k::StopReason::Wall => pb::StopReason::Wall,
        k::StopReason::Denied => pb::StopReason::Denied,
        k::StopReason::Failed => pb::StopReason::Failed,
        k::StopReason::Cancelled => pb::StopReason::Cancelled,
    };
    value as i32
}

fn role_to_i32(role: k::Role) -> i32 {
    let value = match role {
        k::Role::System => pb::Role::System,
        k::Role::User => pb::Role::User,
        k::Role::Assistant => pb::Role::Assistant,
        k::Role::Tool => pb::Role::Tool,
    };
    value as i32
}

fn role_from_i32(value: i32) -> k::Role {
    match pb::Role::try_from(value).unwrap_or(pb::Role::Unspecified) {
        pb::Role::Assistant => k::Role::Assistant,
        pb::Role::Tool => k::Role::Tool,
        pb::Role::System => k::Role::System,
        _ => k::Role::User,
    }
}

fn code_to_i32(code: k::ErrorCode) -> i32 {
    let value = match code {
        k::ErrorCode::Invalid => pb::Code::Invalid,
        k::ErrorCode::NotFound => pb::Code::NotFound,
        k::ErrorCode::Denied => pb::Code::Denied,
        k::ErrorCode::Budget => pb::Code::Budget,
        k::ErrorCode::Timeout => pb::Code::Timeout,
        k::ErrorCode::Unavailable => pb::Code::Unavailable,
        k::ErrorCode::Internal => pb::Code::Internal,
    };
    value as i32
}

fn permission_to_i32(kind: k::PermissionKind) -> i32 {
    let value = match kind {
        k::PermissionKind::AllowOnce => pb::PermissionKind::AllowOnce,
        k::PermissionKind::AllowAlways => pb::PermissionKind::AllowAlways,
        k::PermissionKind::Reject => pb::PermissionKind::Reject,
    };
    value as i32
}

fn purpose_to_i32(purpose: k::Purpose) -> i32 {
    let value = match purpose {
        k::Purpose::Answer => pb::Purpose::Answer,
        k::Purpose::Summary => pb::Purpose::Summary,
        k::Purpose::Repair => pb::Purpose::Repair,
    };
    value as i32
}

fn phase_to_i32(phase: k::AttemptPhase) -> i32 {
    let value = match phase {
        k::AttemptPhase::Preflight => pb::AttemptPhase::Preflight,
        k::AttemptPhase::Execute => pb::AttemptPhase::Execute,
        k::AttemptPhase::CircuitOpen => pb::AttemptPhase::CircuitOpen,
    };
    value as i32
}

fn trigger_to_i32(trigger: k::AttemptTrigger) -> i32 {
    let value = match trigger {
        k::AttemptTrigger::Selection => pb::AttemptTrigger::Selection,
        k::AttemptTrigger::Retry => pb::AttemptTrigger::Retry,
        k::AttemptTrigger::Fallback => pb::AttemptTrigger::Fallback,
    };
    value as i32
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A wire start request, as the kernel wants it.
pub fn spec_from_pb(start: &pb::StartRequest) -> nucleos::engine::SessionSpec {
    let session = start.spec.as_ref();
    nucleos::engine::SessionSpec {
        routing: session.map(|session| k::Routing {
            chain: session.model_chain.clone(),
            retries: session.retries,
        }),
        system: session
            .map(|session| session.system.clone())
            .filter(|system| !system.is_empty()),
        initial: session
            .map(|session| {
                session
                    .initial
                    .iter()
                    .map(message_from_pb)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        tools: session
            .map(|session| session.tools.clone())
            .unwrap_or_default(),
        params: params_from_pb(session.and_then(|session| session.params.as_ref())),
        budget: budget_from_pb(start.budget.as_ref()),
        credit_tokens: start.credit_tokens,
    }
}

/// Generation parameters, from the wire.
pub fn params_from_pb(params: Option<&pb::GenerateParams>) -> nucleos::adapter::GenerateParams {
    use nucleos::adapter::{GenerateParams, ReasoningEffort, TextFormat, ToolChoice};
    let Some(params) = params else {
        return GenerateParams::default();
    };
    GenerateParams {
        max_output_tokens: params.max_output_tokens,
        temperature: params.temperature,
        top_p: params.top_p,
        stop: params.stop.clone(),
        reasoning_effort: match pb::ReasoningEffort::try_from(params.reasoning_effort) {
            Ok(pb::ReasoningEffort::Minimal) => Some(ReasoningEffort::Minimal),
            Ok(pb::ReasoningEffort::Low) => Some(ReasoningEffort::Low),
            Ok(pb::ReasoningEffort::Medium) => Some(ReasoningEffort::Medium),
            Ok(pb::ReasoningEffort::High) => Some(ReasoningEffort::High),
            _ => None,
        },
        tool_choice: match pb::ToolChoice::try_from(params.tool_choice) {
            Ok(pb::ToolChoice::Required) => ToolChoice::Required,
            Ok(pb::ToolChoice::None) => ToolChoice::None,
            _ => ToolChoice::Auto,
        },
        format: match params.format.as_ref() {
            Some(format) => match pb::TextFormatKind::try_from(format.kind) {
                Ok(pb::TextFormatKind::JsonObject) => TextFormat::JsonObject,
                Ok(pb::TextFormatKind::JsonSchema) => TextFormat::JsonSchema {
                    name: format.schema_name.clone(),
                    schema_json: format.schema_json.clone(),
                },
                _ => TextFormat::Text,
            },
            None => TextFormat::Text,
        },
    }
}

/// A budget, from the wire.
pub fn budget_from_pb(budget: Option<&pb::Budget>) -> k::Budget {
    let Some(budget) = budget else {
        return k::Budget::default();
    };
    k::Budget {
        max_steps: budget.max_steps,
        max_tokens: budget.max_tokens,
        max_micros_usd: budget.max_micros_usd,
        max_wall_millis: budget.max_wall_millis,
    }
}

/// A parked verdict, as the kernel wants it.
pub fn verdict_from_pb(verdict: &pb::HitlVerdict) -> nucleos::engine::HitlVerdict {
    nucleos::engine::HitlVerdict {
        tool_call_id: verdict.tool_call_id.clone(),
        option_id: verdict.option_id.clone(),
    }
}
