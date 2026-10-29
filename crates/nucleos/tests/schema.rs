//! The committed schema is the contract; this test is what keeps it true.
//!
//! Regenerate with `NUCLEOS_SCHEMA=1 cargo test -p nucleos --test schema`. Without the variable
//! the test only reads, so CI fails instead of quietly rewriting the file.

#![allow(clippy::unwrap_used)]

use nucleos::wire::schema::committed_path;

#[test]
fn the_committed_schema_matches_the_code() {
    if std::env::var_os("NUCLEOS_SCHEMA").is_some() {
        nucleos::wire::regenerate().unwrap();
        println!("regenerated {}", committed_path().display());
        return;
    }

    let path = committed_path();
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let expected = serde_json::to_string_pretty(&nucleos::wire::document().unwrap()).unwrap();

    assert_eq!(
        committed.trim_end(),
        expected.trim_end(),
        "\n{} is stale: regenerate with NUCLEOS_SCHEMA=1 cargo test -p nucleos --test schema",
        path.display()
    );
}

#[test]
fn every_reference_resolves() {
    // The old schema was eight files whose imports `protoc` checked. JSON Schema has no compiler,
    // so the check has to be written: a `$ref` pointing at nothing is a schema an SDK generator
    // fails on months later, in somebody else's build.
    let document = nucleos::wire::document().unwrap();
    let defs = document["$defs"].as_object().unwrap();

    let mut pending = vec![document.clone()];
    while let Some(node) = pending.pop() {
        match node {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if key == "$ref" {
                        let target = value.as_str().unwrap();
                        let name = target
                            .strip_prefix("#/$defs/")
                            .unwrap_or_else(|| panic!("$ref {target:?} is not a local definition"));
                        assert!(
                            defs.contains_key(name),
                            "$ref {target:?} names no definition"
                        );
                    } else {
                        pending.push(value);
                    }
                }
            }
            serde_json::Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
}

#[test]
fn a_definition_exists_for_every_endpoint_payload() {
    let document = nucleos::wire::document().unwrap();
    let defs = document["$defs"].as_object().unwrap();

    for name in [
        "Event",
        "Message",
        "Ledger",
        "DescribeResponse",
        "StartRequest",
        "StartResponse",
        "VerdictRequest",
        "ErrorBody",
    ] {
        assert!(defs.contains_key(name), "missing definition: {name}");
    }
}

#[test]
fn no_definition_duplicates_another() {
    // Two types with one name is the failure mode no generator reports: it picks one, and the
    // other SDK is wrong forever.
    let document = nucleos::wire::document().unwrap();
    let defs = document["$defs"].as_object().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for name in defs.keys() {
        assert!(seen.insert(name.clone()), "duplicate definition: {name}");
    }
}
