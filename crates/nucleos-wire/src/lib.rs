//! The HTTP/JSON wire contract.
//!
//! Under C1 the kernel's own types *are* the wire shape: `nucleos::contract` derives
//! `JsonSchema`, and this crate assembles the published document from those types plus the
//! request and response envelopes that exist only at the edge.
//!
//! There is deliberately no parallel type tree. The cost of C1 is that a kernel type's
//! serialized shape is frozen — which is why `schema/v1/schema.json` is committed and a test
//! fails when the code and the file disagree.

pub mod api;
pub mod schema;

pub use schema::{document, regenerate};
