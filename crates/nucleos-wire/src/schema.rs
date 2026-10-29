//! The published JSON Schema, generated in one pass.
//!
//! Under C1 the kernel's own types *are* the wire shape: `nucleos::contract` derives
//! `JsonSchema` next to `Serialize`, and this module takes one document from a single root.
//!
//! Generating from one root rather than merging a document per type is not a detail. Merging
//! means schemars describes the same reachable type twice — once as a root, once as a definition
//! of some other root — and those two descriptions are allowed to differ in ways nobody wants to
//! reconcile. One pass has one version of each type by construction.

use crate::api;
use nucleos::contract as k;
use schemars::{JsonSchema, schema_for};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Where the committed document lives, relative to the workspace root.
pub const SCHEMA_PATH: &str = "schema/v1/schema.json";

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
}

/// The published document.
///
/// Deterministic: `serde_json`'s map is ordered, so two runs of the same code produce
/// byte-identical output and a diff means a real change.
pub fn document() -> Result<Value, serde_json::Error> {
    let mut document = serde_json::to_value(schema_for!(Abi))?;
    let object = document.as_object_mut().ok_or_else(|| {
        serde_json::Error::io(std::io::Error::other("schema root is not an object"))
    })?;
    object.insert(
        "$schema".to_owned(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    object.insert(
        "$id".to_owned(),
        json!("https://nucleos.dev/schema/v1/schema.json"),
    );
    object.insert("title".to_owned(), json!("nucleos.v1"));
    object.insert(
        "description".to_owned(),
        json!(
            "The nucleos ABI. Events are server-sent frames whose `event` field is the \
routable type and whose `id` is the sequence number; every other endpoint is JSON over HTTP."
        ),
    );
    Ok(document)
}

/// The path a test writes to and reads from.
#[must_use]
pub fn committed_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(SCHEMA_PATH)
}

/// Write the committed document.
pub fn regenerate() -> std::io::Result<()> {
    let document = document().map_err(std::io::Error::other)?;
    let mut rendered = serde_json::to_string_pretty(&document).map_err(std::io::Error::other)?;
    rendered.push('\n');
    let path = committed_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, rendered)
}
