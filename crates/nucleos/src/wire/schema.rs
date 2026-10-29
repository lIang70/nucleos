//! The published JSON Schema, generated in one pass.
//!
//! Under C1 the kernel's own types *are* the wire shape: `nucleos::contract` derives
//! `JsonSchema` next to `Serialize`, and this module takes one document from a single root.
//!
//! Generating from one root rather than merging a document per type is not a detail. Merging
//! means schemars describes the same reachable type twice — once as a root, once as a definition
//! of some other root — and those two descriptions are allowed to differ in ways nobody wants to
//! reconcile. One pass has one version of each type by construction.

use super::api;
use crate::contract as k;
use schemars::{JsonSchema, schema_for};
use serde_json::Value;

/// The API surface, as a schema root.
///
/// Its properties name the payload of every endpoint; its `$defs` holds each reachable type
/// exactly once. An SDK generator starts here.
#[derive(Debug, JsonSchema)]
pub struct Abi {
    /// What one `GET /v1/sessions/{id}/events` frame carries.
    ///
    /// There is no frame *type*: a server-sent event already gives the type (`event:`) and the
    /// cursor (`id:`), and a second type restating them is a second truth.
    pub event: k::Event,
    /// `GET /v1/describe`.
    pub describe_response: api::DescribeResponse,
    /// `POST /v1/sessions`.
    pub start_request: api::StartRequest,
    /// What that returns.
    pub start_response: api::StartResponse,
    /// `POST /v1/sessions/{id}/verdict`.
    pub verdict_request: api::VerdictRequest,
    /// The body of every failure.
    pub error: api::ErrorBody,
    /// The conversation model, for a client that sends `initial` messages.
    pub message: k::Message,
    /// What a call cost, attempt by attempt.
    pub ledger: k::Ledger,
    /// `POST /v1/sessions/{id}/inference`.
    pub inference_request: api::InferenceRequest,
    /// What that returns.
    pub inference_response: api::InferenceResponse,
    /// `POST /v1/sessions/{id}/tools`.
    pub tool_batch_request: api::ToolBatchRequest,
    /// What that returns.
    pub tool_batch_response: api::ToolBatchResponse,
    /// `POST /v1/sessions/{id}/state`.
    pub state_request: api::StateRequest,
    /// `GET /v1/sessions/{id}`.
    pub session_view: api::SessionView,
    /// `POST /v1/processes`.
    pub process_start_request: api::ProcessStartRequest,
    /// What that answers with.
    pub process_start_response: api::ProcessStartResponse,
    /// `POST /v1/sessions/{id}/stdin`.
    pub stdin_request: api::StdinRequest,
    /// `POST /v1/sessions/{id}/configure`.
    pub config_patch: k::ConfigPatch,
    /// What that returns.
    pub configure_response: api::ConfigureResponse,
    /// Adapter hints, which a session may change while it runs.
    pub hints: k::Hints,
    /// `POST /v1/sessions/{id}/exec`.
    pub exec_request: api::ExecRequest,
    /// What a command answers with.
    pub exec_output: k::ExecOutput,
    /// What a session's commands may touch.
    pub sandbox_policy: k::SandboxPolicy,
    /// What one command asks for on top.
    pub sandbox_permissions: k::SandboxPermissions,
    /// What a session is for.
    pub session_kind: k::SessionKind,
    /// How to start a process session.
    pub process_spec: k::ProcessSpec,
    /// Where a process is.
    pub process_state: k::ProcessState,
}

/// Every named type, keyed by name.
///
/// The `$defs` of a single-root schema: one pass, one version of each type.
///
/// Deterministic: `serde_json`'s map is ordered, so two runs of the same code produce
/// byte-identical output and a diff means a real change.
pub fn definitions() -> Result<Value, serde_json::Error> {
    let document = serde_json::to_value(schema_for!(Abi))?;
    document
        .get("$defs")
        .cloned()
        .ok_or_else(|| serde_json::Error::io(std::io::Error::other("schema has no $defs")))
}
