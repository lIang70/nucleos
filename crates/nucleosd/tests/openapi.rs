//! The published OpenAPI document is the contract an SDK is generated from.
//!
//! It lives here, not in the kernel, because the document is generated from the *route table* —
//! and the route table carries the handlers, which are the daemon's.
//!
//! Regenerate with `NUCLEOS_OPENAPI=1 cargo test -p nucleosd --test openapi`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::wire::openapi;
use nucleosd::http::openapi_routes;
use serde_json::Value;

#[test]
fn the_committed_document_matches_the_code() {
    if std::env::var_os("NUCLEOS_OPENAPI").is_some() {
        openapi::regenerate(&openapi_routes()).unwrap();
        println!("regenerated {}", openapi::committed_path().display());
        return;
    }

    let path = openapi::committed_path();
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let expected =
        serde_json::to_string_pretty(&openapi::document(&openapi_routes()).unwrap()).unwrap();

    assert_eq!(
        committed.trim_end(),
        expected.trim_end(),
        "\n{} is stale: regenerate with NUCLEOS_OPENAPI=1 cargo test -p nucleosd --test openapi",
        path.display()
    );
}

#[test]
fn every_reference_resolves_to_a_component() {
    // A generated client follows these refs; one that points at nothing is a broken SDK, and it
    // breaks in somebody else's build rather than in ours.
    let document = openapi::document(&openapi_routes()).unwrap();
    let schemas = document["components"]["schemas"].as_object().unwrap();

    let mut pending = vec![document.clone()];
    while let Some(node) = pending.pop() {
        match node {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "$ref" {
                        let target = value.as_str().unwrap();
                        let name = target
                            .strip_prefix("#/components/schemas/")
                            .unwrap_or_else(|| panic!("$ref {target:?} is not a local component"));
                        assert!(schemas.contains_key(name), "missing component: {name}");
                    } else {
                        pending.push(value);
                    }
                }
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
}

#[test]
fn every_route_names_a_schema_that_exists() {
    let document = openapi::document(&openapi_routes()).unwrap();
    let schemas = document["components"]["schemas"].as_object().unwrap();

    for name in openapi::referenced_schemas(&openapi_routes()) {
        assert!(
            schemas.contains_key(name),
            "route refers to {name}, which the schema does not define"
        );
    }
}

#[test]
fn every_definition_is_an_object() {
    // The definitions used to be published separately as JSON Schema. They are not any more, so
    // the sanity check that used to live beside that file lives here: one pass over one root
    // means a type can only be described once, and an empty description means the generator
    // stopped finding it.
    let document = openapi::document(&openapi_routes()).unwrap();
    let schemas = document["components"]["schemas"].as_object().unwrap();
    assert!(schemas.len() > 40, "the definitions shrank unexpectedly");
    for (name, schema) in schemas {
        assert!(schema.is_object(), "{name} is not a schema object");
        assert!(
            schema.as_object().is_some_and(|object| !object.is_empty()),
            "{name} is empty"
        );
    }
}

#[test]
fn the_stream_payload_is_the_event() {
    // A generator that only emits referenced components would otherwise drop `Event`, because
    // nothing else points at it. Each frame's `data:` is an Event, so the reference is honest.
    let document = openapi::document(&openapi_routes()).unwrap();
    let events = &document["paths"]["/v1/sessions/{id}/events"]["get"]["responses"]["200"];
    assert_eq!(
        events["content"]["text/event-stream"]["schema"]["$ref"],
        "#/components/schemas/Event"
    );
}

#[test]
fn the_stream_is_documented_as_a_stream() {
    // The one thing OpenAPI cannot express: an SDK generates a client for every other route and
    // hand-writes this one, so it had better say so.
    let document = openapi::document(&openapi_routes()).unwrap();
    let events = &document["paths"]["/v1/sessions/{id}/events"]["get"]["responses"]["200"];
    assert!(events["content"]["text/event-stream"].is_object());
    assert!(
        events["description"]
            .as_str()
            .unwrap()
            .contains("Last-Event-ID")
    );
}

/// Every `{placeholder}` in a path is declared, and nothing else is.
///
/// OpenAPI requires it — a path template with an undeclared variable is invalid, and generators either
/// refuse the document or emit the braces literally — and the client-facing consequence is worse than
/// the validation error: a generated method with no `id` argument.
///
/// The document is generated, so this asserts the generator, not the table: path parameters are derived
/// from the template precisely so a route cannot name one and forget to declare it.
#[test]
fn every_path_parameter_is_declared() {
    let document = openapi::document(&openapi_routes()).unwrap();
    let mut checked = 0;
    for (path, item) in document["paths"].as_object().unwrap() {
        let mut in_template: Vec<String> = Vec::new();
        let mut rest = path.as_str();
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}').expect("a `{` without `}`");
            in_template.push(rest[open + 1..open + close].to_owned());
            rest = &rest[open + close + 1..];
        }

        for (method, operation) in item.as_object().unwrap() {
            let declared: Vec<String> = operation["parameters"]
                .as_array()
                .map(|parameters| {
                    parameters
                        .iter()
                        .filter(|parameter| parameter["in"] == "path")
                        .map(|parameter| parameter["name"].as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                declared, in_template,
                "{method} {path}: the path names {in_template:?}, the document declares {declared:?}"
            );
            // And a path parameter is required by definition; an optional one is a template that may
            // not be filled.
            for parameter in operation["parameters"].as_array().into_iter().flatten() {
                if parameter["in"] == "path" {
                    assert_eq!(parameter["required"], true, "{method} {path}");
                }
            }
            checked += 1;
        }
    }
    assert!(checked > 30, "only {checked} operations checked");
}
