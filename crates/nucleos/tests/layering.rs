//! The module graph is one-way, and this test is what makes that true tomorrow.
//!
//! `contract → kernel → adapter` is asserted twice: once by the order of the `pub mod`
//! declarations, and once by the kernel having no dependency on the wire contract. Both
//! are cheap to check now and expensive to unwind later — a layer that reaches upward
//! couples everything above it to everything below.

use std::fs;
use std::path::Path;

/// The layering is a fact about the source, not about the type checker: Rust lets any
/// module name any other, so the only place to enforce it is here.
#[test]
fn modules_are_declared_bottom_up() {
    let lib = read("src/lib.rs");
    let order: Vec<usize> = ["pub mod adapter;", "pub mod contract;", "pub mod kernel;"]
        .iter()
        .map(|needle| {
            lib.find(needle)
                .unwrap_or_else(|| panic!("src/lib.rs must declare `{needle}`"))
        })
        .collect();

    assert!(
        order[0] < order[1] && order[1] < order[2],
        "modules must be declared contract, kernel, adapter — cheapest dependency first"
    );
}

/// The kernel must never learn the wire. Every abstraction that exists because "the wire
/// needs it" is a constraint it would carry forever, so the manifest is the place to stop
/// it.
#[test]
fn kernel_does_not_depend_on_a_transport() {
    // The wire *format* is the kernel's own types now (that is the trade C1 makes). What must
    // still stay out is the *transport*: a kernel that links a web framework has opinions about
    // sockets, and a host that embeds it inherits them.
    let manifest = read("Cargo.toml");
    for forbidden in ["axum", "hyper", "tower", "tonic", "prost", "nucleos-agent"] {
        assert!(
            !manifest.contains(forbidden),
            "the kernel must not depend on `{forbidden}`: transport belongs in nucleosd, \
and the loop belongs in nucleos-agent"
        );
    }
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}
