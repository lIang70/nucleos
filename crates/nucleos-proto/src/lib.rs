//! The nucleos ABI contract.
//!
//! The contract is `proto/nucleos/v1/` at the repository root — outside every crate,
//! because it is the contract for every SDK, not just this one. Six files, split by reason
//! to change (`common`, `content`, `session`, `event`, `source`, and the service in
//! `kernel.proto`); imports run `{common, content} ← {session, event}`, `common ← source`,
//! everything `← kernel`, and proto3 rejects a cycle, so the layering is checked by
//! `protoc` for free.
//!
//! It is imported by nothing else in the workspace except `nucleosd`: the kernel library
//! must not learn the wire, because every abstraction that exists "because the wire needs
//! it" is a constraint it would carry forever.
//!
//! # Status
//!
//! The `.proto` is the source of truth and is frozen at v1 (see `docs/abi.md` §8). The
//! generated bindings land in P1, together with the build step that runs them; consumers
//! never need `protoc`, because the bindings ship with this crate.
//!
//! That is why the schema lives outside the crate and the output lives inside it: `cargo
//! publish` packages only this directory, so shipping the `.proto` to consumers would be a
//! promise the packaging cannot keep. Generated code is the artefact; the schema is ours.
//!
//! Until then this crate carries only the version constants, so the workspace builds
//! without a code generator on the path.

/// The protobuf package this crate's contract lives in.
pub const PACKAGE: &str = "nucleos.v1";

/// The ABI major version served by this build.
///
/// A breaking change is `nucleos.v2` served side by side, never a bump in place.
pub const ABI_VERSION: &str = "1";
