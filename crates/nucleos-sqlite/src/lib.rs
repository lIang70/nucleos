//! A durable store, in SQLite.
//!
//! The event log is the record and the snapshot is a cache of it: two tables, both append-only,
//! and the rules the kernel relies on are enforced by this crate rather than hoped for. A store
//! that silently renumbered an event would make every later resume cursor wrong, so a gap is
//! refused here.
//!
//! Three deployment decisions are worth stating:
//!
//! - **WAL, synchronous = NORMAL.** A `kill -9` is survived; a power cut may lose the last
//!   commits. The alternative, `FULL`, fsyncs twice per write to buy durability against an event
//!   that already lost the process that produced it.
//! - **One connection behind a mutex, every call on the blocking pool.** rusqlite is synchronous
//!   and a write can wait on the filesystem; running that on the reactor thread would stall every
//!   other session in the process.
//! - **Events are JSON blobs beside two indexed columns.** The kernel owns the schema and derives
//!   it in one place; a second, SQL-shaped copy of `EventKind` would be a second truth that drifts
//!   at the first field rename.

mod store;
mod system;

pub use store::SqliteStore;
pub use system::{SCHEMA_VERSION, SystemStore};
