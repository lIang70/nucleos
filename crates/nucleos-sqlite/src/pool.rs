//! One writer, several readers.
//!
//! A store used to be one `Arc<Mutex<Connection>>`: every read waited behind every write, and a
//! request that only wanted to know *who is calling* waited behind a checkpoint of somebody else's
//! conversation. SQLite's WAL mode exists to remove exactly that wait — readers never block the
//! writer and the writer never blocks readers — but only if the readers have connections of their
//! own. One connection cannot use it.
//!
//! The shape is deliberate:
//!
//! - **One writer.** Every mutation goes through it, so "two writers interleaved a log" stays a
//!   problem this type does not have. WAL allows one writer at a time anyway; making that a type
//!   rather than a lock error is the point.
//! - **Readers are read-only by signature.** [`Pool::read`] hands out `&Connection`, not `&mut`, so
//!   `transaction()` — which needs `&mut` — cannot be called by accident. A read that writes is a
//!   bug the compiler now refuses.
//! - **Work runs on the blocking pool.** rusqlite is synchronous; a read that waits on the
//!   filesystem must not stall the reactor.
//!
//! Consistency needs no extra machinery here: every write in this crate is a single transaction,
//! so a reader sees either the state before it or the state after it, never a half-applied one.
//! There is no interactive transaction held across an `.await`.

use nucleos::error::{Error, Result};
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// How many readers a file-backed store gets.
///
/// Four, not one per core: these are I/O waits, not compute, and the point is that a reader never
/// waits behind a checkpoint. More readers than that buys contention on the same pages.
const READERS: usize = 4;

/// A writer and the readers beside it.
#[derive(Debug)]
pub(crate) struct Pool {
    writer: Arc<Mutex<Connection>>,
    readers: Vec<Arc<Mutex<Connection>>>,
    next: AtomicUsize,
}

impl Pool {
    /// A pool over one already-opened connection, with no readers beside it.
    ///
    /// What an in-memory store gets: there is no WAL to read through, and a second connection to
    /// `:memory:` would be a *different* database. Tests keep the old, simple shape.
    pub(crate) fn single(connection: Connection) -> Self {
        Self {
            writer: Arc::new(Mutex::new(connection)),
            readers: Vec::new(),
            next: AtomicUsize::new(0),
        }
    }

    /// A pool over a file: one writer, and readers opened on the same path.
    ///
    /// The writer is opened and migrated by the caller first, so the readers never see a database
    /// mid-migration — they open a file whose schema is already settled.
    pub(crate) fn file(
        writer: Connection,
        path: &std::path::Path,
        open: impl Fn(&std::path::Path) -> Result<Connection>,
    ) -> Result<Self> {
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            readers.push(Arc::new(Mutex::new(open(path)?)));
        }
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers,
            next: AtomicUsize::new(0),
        })
    }

    /// The writer, for the synchronous paths a store still has.
    pub(crate) fn writer(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a mutation on the blocking pool.
    pub(crate) async fn write<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            let mut guard = connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&mut guard)
        })
        .await
        .map_err(|error| Error::internal(format!("the store task failed: {error}")))?
    }

    /// Run a read on the blocking pool, on a connection of its own.
    ///
    /// Round-robin rather than "first free": finding a free reader would need a lock, which is the
    /// wait this type exists to remove. A reader that is busy is a reader that finishes.
    pub(crate) async fn read<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        // No readers means the single-connection shape: the writer answers, which is what an
        // in-memory store has always done.
        let connection = match self.readers.len() {
            0 => Arc::clone(&self.writer),
            count => {
                let index = self.next.fetch_add(1, Ordering::Relaxed) % count;
                Arc::clone(&self.readers[index])
            }
        };
        tokio::task::spawn_blocking(move || {
            let guard = connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&guard)
        })
        .await
        .map_err(|error| Error::internal(format!("the store task failed: {error}")))?
    }
}

/// The pragmas every connection to a store needs, writer and reader alike.
///
/// `busy_timeout` is the one that matters once there is more than one connection: WAL lets a reader
/// and a writer proceed together, but two writers still serialise, and a lock that is momentarily
/// held should be waited for rather than reported as a failure.
pub(crate) fn configure(connection: &Connection) -> Result<()> {
    connection
        .pragma_update(None, "journal_mode", "wal")
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    connection
        .pragma_update(None, "synchronous", "normal")
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    connection
        .pragma_update(None, "foreign_keys", "on")
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    // Retention deletes rows; without these the file only ever grew. `incremental` lets SQLite return
    // pages to the filesystem gradually instead of requiring a blocking VACUUM, and the WAL is capped
    // so a large prune cannot leave a write-ahead log bigger than the data it deleted.
    connection
        .pragma_update(None, "auto_vacuum", "incremental")
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    connection
        .pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)
        .map_err(|error| Error::internal(format!("store: {error}")))?;
    Ok(())
}
