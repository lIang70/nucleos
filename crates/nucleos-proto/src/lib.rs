//! The nucleos ABI contract.
//!
//! The contract is `proto/nucleos/v1/` at the repository root — outside every crate, because
//! it is the contract for every SDK, not just this one. Eight files, split by reason to
//! change; imports run `{common, content} ← generation ← {event, session, generate}`,
//! `{session, event, generate} ← kernel`, and proto3 rejects a cycle, so the layering is
//! checked by `protoc` for free.
//!
//! It is imported by nothing else in the workspace except `nucleosd`: the kernel library
//! must not learn the wire, because every abstraction that exists "because the wire needs
//! it" is a constraint it would carry forever.
//!
//! # Status
//!
//! The `.proto` is the source of truth and is frozen at v1 (see `docs/abi.md` §8). `build.rs`
//! compiles it; the generated bindings live here and ship with the crate, so consumers never
//! need `protoc` on their machine.
//!
//! That is why the schema lives outside the crate and the output lives inside it: `cargo
//! publish` packages only this directory, so shipping the `.proto` to consumers would be a
//! promise the packaging cannot keep. Generated code is the artefact; the schema is ours.

// Generated code is committed, not produced at build time, so that a consumer can
// `cargo add nucleos-proto` without installing `protoc`. Regenerate with
// `NUCLEOS_REGEN=1 cargo build -p nucleos-proto`.
//
// It is also not ours to lint: every rule in the workspace is about hand-written code, and a
// generated field comment is not a documentation improvement.
#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod pb {
    #![allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]

    include!("generated/nucleos.v1.rs");
}

/// The protobuf package this crate's contract lives in.
pub const PACKAGE: &str = "nucleos.v1";

/// The ABI major version served by this build.
///
/// A breaking change is `nucleos.v2` served side by side, never a bump in place.
pub const ABI_VERSION: &str = "1";
