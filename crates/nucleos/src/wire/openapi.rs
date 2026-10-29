//! The published OpenAPI document.
//!
//! OpenAPI is what this wire actually is: paths, methods, status codes and JSON bodies. The
//! schema this crate already generates becomes its `components.schemas` verbatim — one type
//! definition, one place, no second derivation.
//!
//! The `paths` table is written by hand, deliberately. A derive macro would be a second schema
//! system deriving from the same serde attributes as `schemars`, and two derivations are two
//! things that can disagree about how a type is referenced. Ten rows of data are cheaper than
//! reconciling them.

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Where the committed document lives, relative to the workspace root.
pub const OPENAPI_PATH: &str = "openapi/v1/openapi.json";

/// What a route answers with.
#[derive(Debug, Clone, Copy)]
pub enum Reply {
    /// A JSON body of the named schema.
    Json(&'static str),
    /// No content.
    NoContent,
    /// A server-sent event stream. Not expressible as a schema: an SDK generates a client for
    /// every other route and hand-writes this one.
    Stream,
}

/// One route, as data.
///
/// The table itself lives in the daemon, beside the handler it names: a route's schema and its
/// handler are the same fact, and writing them in two places is how they drift. This struct is the
/// half the kernel can see.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    /// `get`, `post`, …
    pub method: &'static str,
    /// The path, with `{id}` for a parameter.
    pub path: &'static str,
    /// A stable name for generated clients.
    pub operation: &'static str,
    /// One line a human reads in a generated client.
    pub summary: &'static str,
    /// The request body's schema name, when the route takes one.
    pub request: Option<&'static str>,
    /// What it answers with.
    pub reply: Reply,
}

/// The published document.
pub fn document(routes: &[Route]) -> Result<Value, serde_json::Error> {
    let mut schemas = crate::wire::schema::definitions()?;
    // The JSON Schema document points at `#/$defs/…`; inside an OpenAPI document the same types
    // live under `#/components/schemas/…`. Rewriting is the whole of the difference, and doing it
    // here is what lets one generator serve both artefacts.
    rebase(&mut schemas);

    let mut paths = Map::new();
    for route in routes {
        let path = paths
            .entry(route.path.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(operations) = path {
            operations.insert(route.method.to_owned(), operation(route)?);
        }
    }

    Ok(json!({
        "openapi": "3.1.0",
        "info": {
            "title": "nucleos.v1",
            "version": "1",
            "description": "An agent OS kernel. Two layers: `/v1/agents` runs the reference \
    loop, and the `/v1/sessions/{id}/…` routes are the syscalls an application composes into a \
    loop of its own. Every failure is the same `ErrorBody`.",
        },
        // A unix socket cannot be expressed as a URL in OpenAPI. The path is documented rather
        // than faked: a baseUrl of "http://localhost" would be a guess that is wrong by default.
        "servers": [{ "url": "/" , "description": "The daemon's unix socket, or TCP when configured." }],
        "paths": Value::Object(paths),
        "components": { "schemas": schemas },
    }))
}

fn operation(route: &Route) -> Result<Value, serde_json::Error> {
    let mut operation = Map::new();
    operation.insert("operationId".to_owned(), json!(route.operation));
    operation.insert("summary".to_owned(), json!(route.summary));

    if let Some(name) = route.request {
        operation.insert(
            "requestBody".to_owned(),
            json!({
                "required": true,
                "content": { "application/json": { "schema": reference(name) } },
            }),
        );
    }

    let mut responses = Map::new();
    match route.reply {
        Reply::Json(name) => {
            responses.insert(
                "200".to_owned(),
                json!({
                    "description": "The result.",
                    "content": { "application/json": { "schema": reference(name) } },
                }),
            );
        }
        Reply::NoContent => {
            responses.insert("204".to_owned(), json!({ "description": "Done." }));
        }
        Reply::Stream => {
            responses.insert(
            "200".to_owned(),
            json!({
                "description": "One server-sent event per frame. `event:` is the event's type, \
`id:` is its `seq`, `data:` is the event as JSON. Reconnecting with Last-Event-ID replays from \
the next event.",
                    "content": { "text/event-stream": { "schema": reference("Event") } },
                }),
            );
        }
    }
    // One error body for every failure, so a generated client has one branch to write.
    for (status, reason) in [
        (400, "The request was malformed."),
        (403, "Policy refused."),
        (404, "The thing named does not exist."),
        (429, "A budget was exhausted."),
        (500, "A bug."),
        (503, "A dependency was not reachable."),
    ] {
        responses.insert(
            status.to_string(),
            json!({
                "description": reason,
                "content": { "application/json": { "schema": reference("ErrorBody") } },
            }),
        );
    }
    operation.insert("responses".to_owned(), Value::Object(responses));
    Ok(Value::Object(operation))
}

/// Point every internal reference at OpenAPI's component namespace.
fn rebase(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "$ref" {
                    if let Some(target) = value.as_str()
                        && let Some(name) = target.strip_prefix("#/$defs/")
                    {
                        *value = json!(format!("#/components/schemas/{name}"));
                    }
                } else {
                    rebase(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(rebase),
        _ => {}
    }
}

fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// The path a test writes to and reads from.
#[must_use]
pub fn committed_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(OPENAPI_PATH)
}

/// Write the committed document.
pub fn regenerate(routes: &[Route]) -> std::io::Result<()> {
    let document = document(routes).map_err(std::io::Error::other)?;
    let mut rendered = serde_json::to_string_pretty(&document).map_err(std::io::Error::other)?;
    rendered.push('\n');
    let path = committed_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, rendered)
}

/// The schema names a table refers to, so a test can prove they exist.
#[must_use]
pub fn referenced_schemas(routes: &[Route]) -> Vec<&'static str> {
    let mut names = Vec::new();
    for route in routes {
        names.extend(route.request);
        if let Reply::Json(name) = route.reply {
            names.push(name);
        }
    }
    names.push("ErrorBody");
    names
}
