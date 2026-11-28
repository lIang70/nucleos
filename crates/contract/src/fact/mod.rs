//! The record: what a session did, as immutable entries.
//!
//! Two coordinates and one vocabulary. [`Seq`] is a session's dense position, [`BlobId`] is the
//! content address a fact points at instead of repeating bytes, and [`Fact`] is the entry itself.
//!
//! The set of [`FactKind`] variants has no catch-all: a state change that cannot be named cannot
//! be recorded, and an unclassified entry is not a state a reader has to handle.

mod blob;
mod entry;
mod seq;
#[cfg(test)]
mod tests;

pub use blob::BlobId;
pub use entry::{CloseReason, EffectStatus, Fact, FactKind};
pub use seq::Seq;
