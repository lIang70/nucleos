//! Regenerates the committed bindings, and does nothing else.
//!
//! The generated code is committed (`src/generated/nucleos.v1.rs`) so that a consumer can
//! `cargo add nucleos-proto` without installing `protoc` — which is the promise the docs make.
//! That means the build script must not run the generator on every build: it runs only when
//! `NUCLEOS_REGEN=1` is set, which is what a maintainer does after editing the schema.
//!
//! ```text
//! NUCLEOS_REGEN=1 cargo build -p nucleos-proto
//! ```
#![allow(missing_docs)]

use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../proto");
    let proto = root.join("nucleos/v1/kernel.proto");

    // A schema edit must not silently leave the committed bindings stale, so a rebuild is
    // triggered by the schema changing even though nothing is regenerated.
    println!("cargo:rerun-if-changed={}", proto.display());
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-env-changed=NUCLEOS_REGEN");

    if std::env::var_os("NUCLEOS_REGEN").is_none() {
        return;
    }

    let out = manifest.join("src/generated");
    if let Err(error) = std::fs::create_dir_all(&out) {
        panic!("cannot create {}: {error}", out.display());
    }

    if let Err(error) = generate(&proto, &root, &out) {
        panic!("code generation failed: {error}");
    }
}

fn generate(proto: &Path, root: &Path, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .out_dir(out)
        .compile_protos(&[proto], &[root])?;
    Ok(())
}
