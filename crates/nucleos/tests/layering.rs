//! The module graph is one-way, and this test is what makes that true tomorrow.
//!
//! The real direction is `contract ← adapter ← kernel`: the contract knows nothing, an adapter
//! implements a contract and knows nothing about the kernel, and the kernel names both. `wire` sits
//! beside the contract because it is the same types plus HTTP envelopes.
//!
//! Declaration order proves nothing — Rust lets any module name any other, and the order `pub mod`
//! lines happen to be written in constrains nobody. So the direction is checked where it actually
//! lives: in the `use` statements of every source file.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// Modules a layer must not name. Each entry is a directory whose files may not `use` the listed
/// paths, in addition to `crate::kernel`-style names in `crate::contract` form.
const FORBIDDEN: &[(&str, &[&str])] = &[
    // The contract is the bottom: it names nothing above it.
    ("contract.rs", &["crate::kernel", "crate::adapter"]),
    // An adapter implements a contract; it does not know the kernel that drives it.
    ("adapter", &["crate::kernel"]),
];

/// A layer that names something above it is a dependency pointing the wrong way, and everything
/// above it inherits the coupling.
#[test]
fn a_layer_does_not_name_the_one_above_it() {
    for (path, forbidden) in FORBIDDEN {
        for file in sources(path) {
            let text = fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
            // Comments are not dependencies: a doc link to something above this layer is a sentence
            // about the code, and treating it as an edge would make the check impossible to satisfy.
            let text = strip_comments(&text);
            for needle in *forbidden {
                assert!(
                    !text.contains(needle),
                    "{} names `{needle}`: the module graph is contract ← adapter ← kernel, and a \
                     lower layer naming a higher one is the edge that makes it a cycle",
                    file.display()
                );
            }
        }
    }
}

/// The kernel must never depend on the wire contract's transport, and must not depend on a driver.
///
/// The wire *format* is the kernel's own types — that is the trade the schema makes. What must stay
/// out is the transport: a kernel that links a web framework has opinions about sockets, and a host
/// that embeds it inherits them. A driver is the application's loop, and the kernel must not learn it.
#[test]
fn kernel_does_not_depend_on_a_transport_or_a_driver() {
    let manifest = read("Cargo.toml");
    for forbidden in ["axum", "hyper", "tower", "tonic", "prost", "nucleos-agent"] {
        assert!(
            !manifest.contains(forbidden),
            "the kernel must not depend on `{forbidden}`: transport belongs in nucleosd, and the \
             loop belongs to the application"
        );
    }
}

/// The source without its comments.
///
/// Line-oriented on purpose: a doc comment cannot create a dependency, and no real edge in this crate
/// is written inside a string literal.
fn strip_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !(line.starts_with("//") || line.starts_with('*') || line.starts_with("/*"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One module's source: a single file, or every file under a directory.
fn sources(relative: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = root.join(relative);
    if path.is_file() {
        return vec![path];
    }
    let mut files = Vec::new();
    walk(&path, &mut files);
    files
}

fn walk(directory: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            walk(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}
