//! The HTTP/JSON wire contract.
//!
//! Under C1 the kernel's own types *are* the wire shape: [`crate::contract`] derives `JsonSchema`
//! next to `Serialize`, so there is no parallel type tree. What lives here is the part that is
//! genuinely only about the edge — request and response envelopes, the error body, and the
//! assembly of the published document.
//!
//! It is a module rather than a crate because a crate would imply a boundary that no longer
//! exists: the "contract" is not a layer below the kernel, it is the kernel's own types plus a
//! handful of envelopes.
//!
//! The published artefact is `openapi/v1/openapi.json`: the message definitions under
//! `components.schemas`, and the routes beside them. Regenerate it with
//! `NUCLEOS_OPENAPI=1 cargo test -p nucleos --test openapi`.

pub mod api;
pub mod openapi;
pub mod schema;

pub use openapi::regenerate;
pub use schema::definitions;
