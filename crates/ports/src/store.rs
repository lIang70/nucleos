//! Where the record lives.
//!
//! One seam, split so that reading cannot write: [`Journal`] appends entries and [`History`] reads
//! them, and a caller that only needs to answer "what happened" cannot reach `append` by accident.
//!
//! # Append-only, and why the port says so
//!
//! There is no update and no delete. A session's state is its log, so an entry that could be
//! rewritten would be a second source of truth that a restart has to reconcile. An implementation
//! that finds itself wanting `update` is implementing a cache, and a cache belongs beside this
//! seam rather than inside it.
//!
//! # Two reads, two questions
//!
//! [`History::after`] answers "what has this session done that I have not seen", which is a resume
//! cursor. [`History::range`] answers "what happened between these positions", which is an audit
//! page. They are separate calls because a cursor that silently becomes a page size is how a
//! client misses entries.
//!
//! # No implementation here
//!
//! This module is the seam, and a seam with a bundled fixture becomes the fixture: every test would
//! pass against it and the durable path would stay unwritten. The implementation arrives with the
//! adapter that stores the log for real.

use async_trait::async_trait;
use nucleos_contract::fact::{BlobId, Fact, Seq};
use nucleos_contract::{Result, SessionKey};

/// Append entries and store content.
#[async_trait]
pub trait Journal: Send + Sync {
    /// Append one entry.
    ///
    /// The store assigns nothing: the entry already carries its [`Seq`], because the runtime decides
    /// the position and the store records it. A store that chose the position would make two
    /// concurrent writers depend on lock ordering for correctness.
    ///
    /// # Errors
    ///
    /// A position that is not the next one is a [`ErrorKind::Conflict`](nucleos_contract::ErrorKind::Conflict):
    /// the caller's view of the log is behind or ahead, and appending anyway would leave a hole or
    /// overwrite an entry.
    async fn append(&self, fact: &Fact) -> Result<()>;

    /// Store content and return its address.
    ///
    /// Content-addressed, so writing the same bytes twice is one row. Implementations must not
    /// return an address for different bytes than they were given.
    ///
    /// # Errors
    ///
    /// The store could not persist it.
    async fn put(&self, bytes: &[u8]) -> Result<BlobId>;
}

/// Read entries and content.
#[async_trait]
pub trait History: Send + Sync {
    /// Entries strictly after `cursor`, in position order.
    ///
    /// `limit` bounds how many come back; `None` means the implementation's own ceiling, not
    /// "everything". A caller that needs to know whether more exist asks again from the last
    /// position it received.
    ///
    /// # Errors
    ///
    /// The session is unknown, or the store could not be read.
    async fn after(&self, key: &SessionKey, cursor: Seq, limit: Option<u32>) -> Result<Vec<Fact>>;

    /// Entries between two positions, inclusive of both.
    ///
    /// # Errors
    ///
    /// The session is unknown, or the store could not be read.
    async fn range(&self, key: &SessionKey, from: Seq, to: Seq) -> Result<Vec<Fact>>;

    /// The most recent position, or `None` when the session has no entries.
    ///
    /// A separate call because it is the one question a resume asks before reading: an empty answer
    /// and a session that does not exist are different, and a caller that learned both from an empty
    /// `after` result could not tell them apart.
    ///
    /// # Errors
    ///
    /// The store could not be read.
    async fn head(&self, key: &SessionKey) -> Result<Option<Seq>>;

    /// The content at an address.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](nucleos_contract::ErrorKind::NotFound) when no content was stored
    /// under that address.
    async fn get(&self, blob: BlobId) -> Result<Vec<u8>>;
}

/// Both halves, for a store that serves reading and appending.
///
/// A blanket implementation rather than a supertrait: a test that only reads can implement
/// [`History`] alone, and the runtime that needs both can ask for `dyn Store`.
pub trait Store: Journal + History {}

impl<T: Journal + History> Store for T {}
