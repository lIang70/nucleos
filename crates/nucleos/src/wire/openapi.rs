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
    /// Either, chosen by `Accept`.
    ///
    /// One route with two answers is a fact about the route, not two routes: the request, the
    /// authentication and the commit point are identical, and a client that documents only one of
    /// them cannot ask for the other.
    JsonOrStream(&'static str),
}

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// In the query string.
    Query,
    /// In a header.
    Header,
}

/// A parameter's type, as the document describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string.
    String,
    /// A non-negative integer.
    Integer,
}

/// A query or header parameter.
///
/// Path parameters are **not** here: they are derived from the `{placeholder}` in the route's path,
/// so a route cannot name one and forget to declare it.
#[derive(Debug, Clone, Copy)]
pub struct Parameter {
    /// Its name, exactly as the handler reads it.
    pub name: &'static str,
    /// Where it travels.
    pub location: Location,
    /// Its type.
    pub kind: Kind,
    /// One line a generated client shows its caller.
    pub description: &'static str,
}

impl Parameter {
    /// A query parameter.
    #[must_use]
    pub const fn query(name: &'static str, kind: Kind, description: &'static str) -> Self {
        Self {
            name,
            location: Location::Query,
            kind,
            description,
        }
    }

    /// A header parameter.
    #[must_use]
    pub const fn header(name: &'static str, kind: Kind, description: &'static str) -> Self {
        Self {
            name,
            location: Location::Header,
            kind,
            description,
        }
    }

    fn json(self) -> Value {
        json!({
            "name": self.name,
            "in": match self.location {
                Location::Query => "query",
                Location::Header => "header",
            },
            // A query or header the handler treats as optional: absent means "use the default",
            // and saying otherwise would make every generated client demand it.
            "required": false,
            "schema": {
                "type": match self.kind {
                    Kind::String => "string",
                    Kind::Integer => "integer",
                }
            },
            "description": self.description,
        })
    }
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
    /// Query and header parameters. Path parameters come from the path itself.
    pub parameters: &'static [Parameter],
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

    let mut parameters: Vec<Value> = placeholders(route.path)
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
                "description": path_description(&name),
            })
        })
        .collect();
    parameters.extend(route.parameters.iter().copied().map(Parameter::json));
    if !parameters.is_empty() {
        operation.insert("parameters".to_owned(), json!(parameters));
    }

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
        Reply::JsonOrStream(name) => {
            responses.insert(
                "200".to_owned(),
                json!({
                    "description": "JSON, or the turn's events when `Accept: text/event-stream`. \
The last frame is the assembled message, so a client that ignores the others gets the same \
answer.",
                    "content": {
                        "application/json": { "schema": reference(name) },
                        "text/event-stream": { "schema": reference("Event") },
                    },
                }),
            );
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

/// The `{name}` placeholders in a path, in order.
fn placeholders(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        names.push(rest[open + 1..open + close].to_owned());
        rest = &rest[open + close + 1..];
    }
    names
}

/// One line for a path parameter, by name.
///
/// The names are few and the sentences matter to whoever reads a generated client: "The session this
/// addresses" is worth writing three times.
fn path_description(name: &str) -> &'static str {
    match name {
        "id" => "The session this addresses.",
        "tenant" => "The tenant this addresses.",
        "name" => "The model or source this names.",
        _ => "Named in the path.",
    }
}
