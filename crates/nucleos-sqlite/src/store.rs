//! The SQLite store.

use crate::pool::{Pool, configure};
use async_trait::async_trait;
use nucleos::adapter::{SessionKey, SessionPage, SessionQuery, Store};
use nucleos::contract::{Event, SessionId, SessionKind, Snapshot};
use nucleos::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Arc;

/// A store backed by one SQLite database file.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: Arc<Pool>,
}

impl SqliteStore {
    /// Open, or create, a database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::from_connection(Connection::open(path).map_err(failed)?, Some(path))
    }

    /// A database in memory.
    ///
    /// The same code path as a file, so a test against it exercises the real store rather than a
    /// second implementation that agrees with it by accident. Each call is an independent database,
    /// which is what a test wants.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(failed)?, None)
    }

    fn from_connection(mut connection: Connection, path: Option<&Path>) -> Result<Self> {
        // WAL is ignored for in-memory databases, which is fine: there is nothing to recover.
        configure(&connection)?;
        connection
            .execute_batch(
                // Every key is (tenant, session), not the session alone. Session identifiers are
                // chosen by callers, and two tenants each choosing "s1" is ordinary rather than
                // adversarial — so isolation lives in the schema, where a lookup cannot forget it.
                "create table if not exists events (
                     tenant  text    not null,
                     session text    not null,
                     seq     integer not null,
                     body    text    not null,
                     primary key (tenant, session, seq)
                 ) without rowid;

                 create table if not exists snapshots (
                     tenant  text    not null,
                     session text    not null,
                     seq     integer not null,
                     -- The version, beside the body rather than inside it: the compare-and-swap on
                     -- every checkpoint read it out of the JSON, which meant parsing a document the
                     -- size of the conversation to compare one integer.
                     version integer not null default 0,
                     -- How many rows of `messages` and `attempts` this checkpoint has already written.
                     -- The checkpoint appends the difference; without these two it would rewrite every
                     -- message on every turn, which is the cost this schema exists to remove.
                     messages_held integer not null default 0,
                     attempts_held integer not null default 0,
                     body    text    not null,
                     primary key (tenant, session)
                 ) without rowid;

                 -- The conversation and the attempt ledger, one row each, written once. They live
                 -- outside the snapshot for the same reason the version does: the snapshot is
                 -- rewritten at every turn boundary, and a rewrite may not be proportional to a
                 -- session's age. The body no longer carries either, so it is the head a view reads.
                 create table if not exists messages (
                     tenant  text    not null,
                     session text    not null,
                     ordinal integer not null,
                     body    text    not null,
                     primary key (tenant, session, ordinal)
                 ) without rowid;
                 create table if not exists attempts (
                     tenant  text    not null,
                     session text    not null,
                     ordinal integer not null,
                     body    text    not null,
                     primary key (tenant, session, ordinal)
                 ) without rowid;

                 -- The listing's own table: a session exists once it has an event, and its kind is
                 -- only known once a checkpoint says so. Keeping it separate from the log means a
                 -- page of ids does not read a single event body.
                 create table if not exists sessions (
                     tenant  text    not null,
                     session text    not null,
                     kind    text,
                     subject text,
                     -- Last activity, for retention. Null means unknown, and a session we
                     -- cannot date is never pruned: retention deletes what it can date, not what it
                     -- cannot.
                     last_activity_unix_millis integer,
                     -- Whether the checkpoint says this session is closed. Only closed sessions are
                     -- ever pruned; an open one is work in progress, however old it is.
                     closed  integer not null default 0,
                     primary key (tenant, session)
                 ) without rowid;
                 create index if not exists sessions_by_kind on sessions (tenant, kind, session);

                 -- The session store's own version, separate from system.db's: the two files have two
                 -- lifecycles, and a version that covers both would refuse an upgrade of one because
                 -- the other had not caught up.
                 create table if not exists meta (
                     key   text primary key,
                     value text not null
                 );",
            )
            .map_err(failed)?;
        // Additive migration for a database written before `subject` existed. The listing can filter
        // by a column that is null for those rows, which is honest: an old row's owner is unknown
        // here, and `snapshot` still knows it.
        let has_subject = connection
            .prepare("pragma table_info(sessions)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .and_then(|names| {
                        let mut found = false;
                        for name in names {
                            if name? == "subject" {
                                found = true;
                            }
                        }
                        Ok(found)
                    })
            })
            .map_err(failed)?;
        if !has_subject {
            connection
                .execute_batch("alter table sessions add column subject text")
                .map_err(failed)?;
        }
        // The same additive migration for retention's two columns. Activity is backfilled from each
        // session's last event, and `closed` from its checkpoint; a row we cannot date stays NULL and
        // is therefore never pruned.
        let session_columns: Vec<String> = connection
            .prepare("pragma table_info(sessions)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .and_then(|names| names.collect::<std::result::Result<_, _>>())
            })
            .map_err(failed)?;
        for (column, ddl) in [
            (
                "last_activity_unix_millis",
                "alter table sessions add column last_activity_unix_millis integer",
            ),
            (
                "closed",
                "alter table sessions add column closed integer not null default 0",
            ),
        ] {
            if !session_columns.iter().any(|name| name == column) {
                connection.execute_batch(ddl).map_err(failed)?;
            }
        }
        connection
            .execute_batch(
                "update sessions set last_activity_unix_millis = (
                     select json_extract(e.body, '$.at_unix_millis') from events e
                     where e.tenant = sessions.tenant and e.session = sessions.session
                     order by e.seq desc limit 1
                 ) where last_activity_unix_millis is null;
                 update sessions set closed = 1 where exists (
                     select 1 from snapshots s
                     where s.tenant = sessions.tenant and s.session = sessions.session
                       and json_extract(s.body, '$.state') = 'closed'
                 );",
            )
            .map_err(failed)?;
        // A database written by a newer build is refused before anything below runs: a migration that
        // rewrites rows into a shape it does not understand is how a downgrade corrupts a durable
        // record. The check is repeated in `check_version`, which is the one that records a version.
        if let Some(version) = read_version(&connection)?
            && version > SCHEMA_VERSION
        {
            return Err(Error::invalid(format!(
                "session store is schema {version}, this build understands {SCHEMA_VERSION}; \
                 upgrade nucleosd rather than downgrading the store"
            )));
        }

        // The checkpoint's columns, added one at a time so a database written by any earlier build
        // arrives at the same shape. `version` is backfilled from the body, because a version of 0 is
        // not a version this code writes (the first checkpoint is 1) and leaving it there would let a
        // stale handle overwrite a newer snapshot.
        let columns: Vec<String> = connection
            .prepare("pragma table_info(snapshots)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .and_then(|names| names.collect::<std::result::Result<_, _>>())
            })
            .map_err(failed)?;
        for (column, ddl) in [
            (
                "version",
                "alter table snapshots add column version integer not null default 0",
            ),
            (
                "messages_held",
                "alter table snapshots add column messages_held integer not null default 0",
            ),
            (
                "attempts_held",
                "alter table snapshots add column attempts_held integer not null default 0",
            ),
        ] {
            if !columns.iter().any(|name| name == column) {
                connection.execute_batch(ddl).map_err(failed)?;
            }
        }
        connection
            .execute_batch(
                "update snapshots set version = json_extract(body, '$.version') where version = 0",
            )
            .map_err(failed)?;
        split_checkpoints(&mut connection)?;
        let store = Self {
            pool: Arc::new(match path {
                // An in-memory store has no WAL to read through, and a second connection to
                // `:memory:` would be a different database. One connection, as it has always been.
                None => Pool::single(connection),
                Some(path) => Pool::file(connection, path, |path| {
                    let reader = Connection::open(path).map_err(failed)?;
                    configure(&reader)?;
                    Ok(reader)
                })?,
            }),
        };
        store.check_version()?;
        Ok(store)
    }

    /// Run `work` on the blocking pool with the connection.
    async fn run<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.pool.write(work).await
    }

    /// Run a read on the blocking pool, on a reader of its own.
    ///
    /// A view that polls while another session checkpoints is the case this exists for: WAL lets the
    /// two proceed together, and one connection behind a mutex was what stopped them.
    async fn read<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.pool.read(work).await
    }
}

/// Rows of stored bodies, as events.
fn read_events(rows: impl Iterator<Item = rusqlite::Result<String>>) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for body in rows {
        let body = body.map_err(failed)?;
        events
            .push(serde_json::from_str(&body).map_err(|error| {
                Error::internal(format!("stored event is unreadable: {error}"))
            })?);
    }
    Ok(events)
}

#[async_trait]
impl Store for SqliteStore {
    async fn append(&self, session: &SessionKey, events: &[Event]) -> Result<()> {
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        let events = events.to_vec();
        self.run(move |connection| {
            let transaction = connection.transaction().map_err(failed)?;
            // In a build with assertions — which is every test — the store checks that the batch
            // really continues the log. It caught a shared-counter race once, and it costs one query
            // per append. In release the caller's sequence is trusted: the kernel reserves it from the
            // log's head under the append lock, and the primary key rejects a duplicate. A gap *inside*
            // the batch is still checked either way.
            let mut expected = if cfg!(debug_assertions) {
                let next: Option<i64> = transaction
                    .query_row(
                        "select max(seq) + 1 from events where tenant = ?1 and session = ?2",
                        params![tenant, session.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(failed)?;
                next.map_or(0, |seq| u64::try_from(seq).unwrap_or(u64::MAX))
            } else {
                events.first().map_or(0, |event| event.seq)
            };

            for event in &events {
                if event.session != session {
                    return Err(Error::invalid(format!(
                        "event for {} appended to {session}",
                        event.session
                    )));
                }
                if event.seq != expected {
                    return Err(Error::invalid(format!(
                        "event seq {} does not continue the log at {expected}",
                        event.seq
                    )));
                }
                let body = serde_json::to_string(event)
                    .map_err(|error| Error::internal(format!("event is not storable: {error}")))?;
                transaction
                    .execute(
                        "insert into events (tenant, session, seq, body) values (?1, ?2, ?3, ?4)",
                        params![
                            tenant,
                            session.as_str(),
                            i64::try_from(event.seq).unwrap_or(i64::MAX),
                            body
                        ],
                    )
                    .map_err(failed)?;
                expected = expected.saturating_add(1);
            }

            // The session exists from its first event; its kind waits for a checkpoint. Activity is
            // recorded here because retention is about *when it was last used*, not when it was
            // created — and this is the only write that knows that without reading the log back.
            let latest = events.iter().map(|event| event.at_unix_millis).max();
            if let Some(latest) = latest {
                transaction
                    .execute(
                        "insert into sessions (tenant, session, kind, last_activity_unix_millis)
                         values (?1, ?2, null, ?3)
                         on conflict (tenant, session) do update set
                           last_activity_unix_millis = max(
                             coalesce(sessions.last_activity_unix_millis, 0),
                             excluded.last_activity_unix_millis
                           )",
                        params![
                            tenant,
                            session.as_str(),
                            i64::try_from(latest).unwrap_or(i64::MAX)
                        ],
                    )
                    .map_err(failed)?;
            }
            transaction.commit().map_err(failed)
        })
        .await
    }

    async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage> {
        let query = query.clone();
        self.read(move |connection| {
            // One row past the page, so "is there more" costs nothing extra.
            let limit = i64::try_from(query.limit.saturating_add(1)).unwrap_or(i64::MAX);
            let after = query.after.as_ref().map(SessionId::as_str).unwrap_or("");
            let kind = query.kind.map(kind_name);

            let mut statement = connection
                .prepare(
                    "select session from sessions
                     where tenant = ?1 and session > ?2
                       and (?3 is null or kind = ?3)
                       and (?5 is null or subject = ?5)
                     order by session limit ?4",
                )
                .map_err(failed)?;
            let rows = statement
                .query_map(
                    params![query.tenant, after, kind, limit, query.subject],
                    |row| row.get::<_, String>(0),
                )
                .map_err(failed)?;
            let mut ids: Vec<String> = rows
                .collect::<std::result::Result<_, _>>()
                .map_err(failed)?;

            let more = ids.len() > query.limit;
            ids.truncate(query.limit);
            let sessions: Vec<SessionId> = ids.into_iter().map(SessionId::new).collect();
            // The cursor is the last id *included* and means "continue strictly after this": a
            // caller that short-circuits the page keeps the row it names.
            let next = more.then(|| sessions.last().cloned()).flatten();
            Ok(SessionPage { sessions, next })
        })
        .await
    }

    async fn snapshot(&self, session: &SessionKey) -> Result<Option<Snapshot>> {
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        self.read(move |connection| {
            // One read transaction over the body and its rows, opened *before* the body is read.
            // Without it the two reads are two snapshots of the database: a checkpoint committed
            // between them leaves the body from one version and the messages from the next, which is
            // a session that never existed.
            let reading = connection.unchecked_transaction().map_err(failed)?;
            let row: Option<(String, i64)> = reading
                .query_row(
                    "select body, version from snapshots where tenant = ?1 and session = ?2",
                    params![tenant, session.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(failed)?;
            let Some((body, version)) = row else {
                return Ok(None);
            };
            let mut snapshot: Snapshot = serde_json::from_str(&body).map_err(|error| {
                Error::invalid(format!("stored snapshot is unreadable: {error}"))
            })?;
            snapshot.version = u64::try_from(version).unwrap_or(u64::MAX);
            // The two vectors the body leaves out, reassembled in the order they were written. The
            // checkpoint is not a document any more; it is a row plus its rows.
            snapshot.messages = rows_in(&reading, &tenant, session.as_str(), "messages")?;
            snapshot.ledger.attempts = rows_in(&reading, &tenant, session.as_str(), "attempts")?;
            reading.commit().map_err(failed)?;
            Ok(Some(snapshot))
        })
        .await
    }

    async fn snapshot_head(&self, session: &SessionKey) -> Result<Option<Snapshot>> {
        // The body no longer carries the conversation or the ledger, so the head is the body: one
        // small row, and nothing parsed that grows with the session's age.
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        self.read(move |connection| {
            let row: Option<(String, i64)> = connection
                .query_row(
                    "select body, version from snapshots where tenant = ?1 and session = ?2",
                    params![tenant, session.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(failed)?;
            let Some((body, version)) = row else {
                return Ok(None);
            };
            let mut snapshot: Snapshot = serde_json::from_str(&body).map_err(|error| {
                Error::invalid(format!("stored snapshot is unreadable: {error}"))
            })?;
            snapshot.version = u64::try_from(version).unwrap_or(u64::MAX);
            Ok(Some(snapshot))
        })
        .await
    }

    async fn write_snapshot(&self, mut snapshot: Snapshot) -> Result<()> {
        self.run(move |connection| {
            // Where the log ends, not how many rows it has. `count(*)` walked every event this session
            // ever wrote on every checkpoint; `max(seq)` is a descent into the primary key. The two
            // numbers agree because the log is dense — the store refuses a gap on append — and a dense
            // log's end is its length.
            let held: i64 = connection
                .query_row(
                    "select coalesce(max(seq) + 1, 0) from events where tenant = ?1 and session = ?2",
                    params![snapshot.principal.tenant, snapshot.session.as_str()],
                    |row| row.get(0),
                )
                .map_err(failed)?;
            let held = u64::try_from(held).unwrap_or(u64::MAX);
            // A snapshot that claims to include more than the log holds would make recovery skip
            // events that exist, and the session would resume having silently lost them.
            if snapshot.seq >= held {
                return Err(Error::invalid(format!(
                    "snapshot claims seq {} but the log holds {held} events",
                    snapshot.seq
                )));
            }

            // The two vectors that grow with a session's age are taken out of the body and written as
            // rows of their own. A turn appends a message and an attempt; the checkpoint writes those,
            // not the whole conversation.
            let messages = std::mem::take(&mut snapshot.messages);
            let attempts = std::mem::take(&mut snapshot.ledger.attempts);
            let body = serde_json::to_string(&snapshot)
                .map_err(|error| Error::internal(format!("snapshot is not storable: {error}")))?;
            let transaction = connection.transaction().map_err(failed)?;

            // Strictly newer, or nothing. Equal versions mean two handles loaded the same snapshot and
            // both think they are the next writer: the first is right and the second is working from a
            // log it read before the first one's work existed. Accepting it would overwrite that work
            // with a materialisation that never saw it.
            let stored: Option<(i64, i64, i64)> = transaction
                .query_row(
                    "select version, messages_held, attempts_held from snapshots
                     where tenant = ?1 and session = ?2",
                    params![snapshot.principal.tenant, snapshot.session.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(failed)?;
            let (stored, held_messages, held_attempts) =
                stored.map_or((0, 0, 0), |(version, messages, attempts)| {
                    (
                        u64::try_from(version).unwrap_or(u64::MAX),
                        u64::try_from(messages).unwrap_or(u64::MAX),
                        u64::try_from(attempts).unwrap_or(u64::MAX),
                    )
                });
            if snapshot.version <= stored {
                return Err(Error::new(
                    nucleos::contract::ErrorCode::Conflict,
                    format!(
                        "snapshot version {} is not newer than the stored {stored}: this handle read \
                         the session before another writer advanced it",
                        snapshot.version
                    ),
                ));
            }
            // A checkpoint adds to a conversation; it never takes away from one. A handle carrying
            // fewer messages than the store holds is working from a snapshot it did not read, and
            // writing it would delete somebody's history.
            if u64::try_from(messages.len()).unwrap_or(u64::MAX) < held_messages {
                return Err(Error::invalid(format!(
                    "checkpoint carries {} messages but the store holds {held_messages}: a checkpoint \
                     may add to a conversation, never remove from it",
                    messages.len()
                )));
            }
            if u64::try_from(attempts.len()).unwrap_or(u64::MAX) < held_attempts {
                return Err(Error::invalid(format!(
                    "checkpoint carries {} attempts but the store holds {held_attempts}: a checkpoint \
                     may add to a ledger, never remove from it",
                    attempts.len()
                )));
            }

            append_rows(
                &transaction,
                &snapshot.principal.tenant,
                snapshot.session.as_str(),
                "messages",
                &messages,
                held_messages,
            )?;
            append_rows(
                &transaction,
                &snapshot.principal.tenant,
                snapshot.session.as_str(),
                "attempts",
                &attempts,
                held_attempts,
            )?;
            transaction
                .execute(
                    "insert into snapshots
                        (tenant, session, seq, version, messages_held, attempts_held, body)
                     values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     on conflict (tenant, session) do update set seq = excluded.seq,
                                                                 version = excluded.version,
                                                                 messages_held = excluded.messages_held,
                                                                 attempts_held = excluded.attempts_held,
                                                                 body = excluded.body",
                    params![
                        snapshot.principal.tenant,
                        snapshot.session.as_str(),
                        i64::try_from(snapshot.seq).unwrap_or(i64::MAX),
                        i64::try_from(snapshot.version).unwrap_or(i64::MAX),
                        i64::try_from(messages.len()).unwrap_or(i64::MAX),
                        i64::try_from(attempts.len()).unwrap_or(i64::MAX),
                        body
                    ],
                )
                .map_err(failed)?;
            transaction
                .execute(
                    "insert into sessions (tenant, session, kind, subject, closed)
                     values (?1, ?2, ?3, ?4, ?5)
                     on conflict (tenant, session) do update set kind = excluded.kind,
                                                                 subject = excluded.subject,
                                                                 closed = excluded.closed",
                    params![
                        snapshot.principal.tenant,
                        snapshot.session.as_str(),
                        kind_name(snapshot.kind),
                        snapshot.principal.subject.clone(),
                        snapshot.state == nucleos::contract::SessionState::Closed
                    ],
                )
                .map_err(failed)?;
            transaction.commit().map_err(failed)
        })
        .await
    }

    async fn prune_closed(
        &self,
        query: &nucleos::adapter::PruneQuery,
    ) -> Result<nucleos::adapter::PruneReport> {
        let query = query.clone();
        if query.dry_run {
            return self
                .read(move |connection| candidates(connection, &query))
                .await;
        }
        self.run(move |connection| {
            let transaction = connection.transaction().map_err(failed)?;
            let report = candidates(&transaction, &query)?;
            for key in &report.candidates {
                for table in ["events", "snapshots", "messages", "attempts", "sessions"] {
                    transaction
                        .execute(
                            &format!("delete from {table} where tenant = ?1 and session = ?2"),
                            params![key.tenant, key.id.as_str()],
                        )
                        .map_err(failed)?;
                }
            }
            transaction.commit().map_err(failed)?;
            Ok(report)
        })
        .await
    }

    async fn delete(&self, session: &SessionKey) -> Result<bool> {
        let (tenant, id) = (session.tenant.clone(), session.id.clone());
        self.run(move |connection| {
            // One transaction: a half-deleted session is a listing entry with no log, which reads as a
            // session that lost its history rather than one that was removed.
            let transaction = connection.transaction().map_err(failed)?;
            let mut removed = 0u64;
            for table in ["events", "snapshots", "messages", "attempts", "sessions"] {
                removed += u64::try_from(
                    transaction
                        .execute(
                            &format!("delete from {table} where tenant = ?1 and session = ?2"),
                            params![tenant, id.as_str()],
                        )
                        .map_err(failed)?,
                )
                .unwrap_or(0);
            }
            transaction.commit().map_err(failed)?;
            Ok(removed > 0)
        })
        .await
    }

    async fn events_page(
        &self,
        session: &SessionKey,
        from_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let (tenant, id) = (session.tenant.clone(), session.id.clone());
        self.read(move |connection| {
            let mut statement = connection
                .prepare(
                    "select body from events where tenant = ?1 and session = ?2 and seq >= ?3
                     order by seq limit ?4",
                )
                .map_err(failed)?;
            read_events(
                statement
                    .query_map(
                        params![
                            tenant,
                            id.as_str(),
                            i64::try_from(from_seq).unwrap_or(i64::MAX),
                            i64::try_from(limit).unwrap_or(i64::MAX)
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(failed)?,
            )
        })
        .await
    }

    async fn events_before(
        &self,
        session: &SessionKey,
        before_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let (tenant, id) = (session.tenant.clone(), session.id.clone());
        self.read(move |connection| {
            let mut statement = connection
                .prepare(
                    "select body from events where tenant = ?1 and session = ?2 and seq < ?3
                     order by seq desc limit ?4",
                )
                .map_err(failed)?;
            let mut events = read_events(
                statement
                    .query_map(
                        params![
                            tenant,
                            id.as_str(),
                            i64::try_from(before_seq).unwrap_or(i64::MAX),
                            i64::try_from(limit).unwrap_or(i64::MAX)
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(failed)?,
            )?;
            // Descending on the way out of SQLite, ascending to the reader: the folder wants one
            // direction and every other reader in this file gets the other.
            events.reverse();
            Ok(events)
        })
        .await
    }

    async fn events_since(&self, session: &SessionKey, seq: u64) -> Result<Vec<Event>> {
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        self.read(move |connection| {
            let mut statement = connection
                .prepare(
                    "select body from events where tenant = ?1 and session = ?2 and seq >= ?3 \
                     order by seq",
                )
                .map_err(failed)?;
            let rows = statement
                .query_map(
                    params![
                        tenant,
                        session.as_str(),
                        i64::try_from(seq).unwrap_or(i64::MAX)
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(failed)?;
            read_events(rows)
        })
        .await
    }
}

/// The column form of a session kind.
///
/// A string, not the enum's ordinal: an ordinal is a schema detail that changes when a variant is
/// inserted, and the database outlives the binary that wrote it.
fn kind_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Agent => "agent",
        SessionKind::Process => "process",
    }
}

/// Turn a driver error into ours, and say so.
///
/// A store failure usually reaches a caller as a single `Err`, which then gets wrapped, returned and
/// forgotten. Logging it here is the one place that still knows it was the *database*.
fn failed(error: rusqlite::Error) -> Error {
    tracing::error!(event = "store_failed", detail = %error);
    Error::internal(format!("store: {error}"))
}

/// The session store's schema version.
///
/// 1 — events and snapshots, the snapshot body carrying everything.
/// 2 — the checkpoint's `version` as a column, and a side row holding the body without its conversation.
/// 3 — the conversation and the attempt ledger as rows of their own, so a checkpoint is proportional to
/// what changed rather than to the session's age, and the body is the head a view reads.
pub const SCHEMA_VERSION: u32 = 3;

/// Which closed sessions a retention pass would remove, and what they hold.
///
/// The count is per candidate rather than one `group by`: the limit is small by design, and this
/// keeps the selection and the deletion looking at exactly the same rows.
fn candidates(
    connection: &Connection,
    query: &nucleos::adapter::PruneQuery,
) -> Result<nucleos::adapter::PruneReport> {
    let mut statement = connection
        .prepare(
            "select s.tenant, s.session,
                    (select count(*) from events e
                     where e.tenant = s.tenant and e.session = s.session)
             from sessions s
             where s.closed = 1
               and s.last_activity_unix_millis is not null
               and s.last_activity_unix_millis < ?1
               and (?2 is null or s.tenant = ?2)
             order by s.last_activity_unix_millis asc, s.tenant, s.session
             limit ?3",
        )
        .map_err(failed)?;
    let rows = statement
        .query_map(
            params![
                i64::try_from(query.before_unix_millis).unwrap_or(i64::MAX),
                query.tenant,
                i64::try_from(query.limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(failed)?;
    let mut report = nucleos::adapter::PruneReport::default();
    for row in rows {
        let (tenant, id, events) = row.map_err(failed)?;
        report.sessions += 1;
        report.events += u64::try_from(events).unwrap_or(0);
        report.candidates.push(nucleos::adapter::SessionKey::new(
            tenant,
            nucleos::contract::SessionId::new(id),
        ));
    }
    Ok(report)
}

/// One column of a checkpoint, in the order it was written.
fn rows_in<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    tenant: &str,
    session: &str,
    table: &str,
) -> Result<Vec<T>> {
    let mut statement = connection
        .prepare(&format!(
            "select body from {table} where tenant = ?1 and session = ?2 order by ordinal"
        ))
        .map_err(failed)?;
    let rows = statement
        .query_map(params![tenant, session], |row| row.get::<_, String>(0))
        .map_err(failed)?;
    let mut values = Vec::new();
    for body in rows {
        let body = body.map_err(failed)?;
        values.push(serde_json::from_str(&body).map_err(|error| {
            Error::invalid(format!("stored {table} row is unreadable: {error}"))
        })?);
    }
    Ok(values)
}

/// Write the rows a checkpoint added, and only those.
///
/// `held` is how many the store already has; a snapshot carries the same rows as a prefix plus whatever
/// the session appended since, so appending the difference is both correct and the point.
fn append_rows<T: serde::Serialize>(
    transaction: &rusqlite::Transaction<'_>,
    tenant: &str,
    session: &str,
    table: &str,
    values: &[T],
    held: u64,
) -> Result<()> {
    let mut statement = transaction
        .prepare(&format!(
            "insert into {table} (tenant, session, ordinal, body) values (?1, ?2, ?3, ?4)"
        ))
        .map_err(failed)?;
    let held = usize::try_from(held).unwrap_or(usize::MAX);
    for (ordinal, value) in values.iter().enumerate().skip(held) {
        let body = serde_json::to_string(value)
            .map_err(|error| Error::internal(format!("a {table} row is not storable: {error}")))?;
        statement
            .execute(params![
                tenant,
                session,
                i64::try_from(ordinal).unwrap_or(i64::MAX),
                body
            ])
            .map_err(failed)?;
    }
    Ok(())
}

/// Move every checkpoint written before version 3 into the shape version 3 reads.
///
/// The conversation and the attempt ledger used to live inside the snapshot body. A body that still
/// carries either is a row written by an older build; one that carries neither parses to the empty
/// vectors and is skipped, which is what makes this idempotent and independent of the version number.
fn split_checkpoints(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction().map_err(failed)?;
    let rows: Vec<(String, String, String)> = {
        let mut statement = transaction
            .prepare("select tenant, session, body from snapshots")
            .map_err(failed)?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .and_then(|rows| rows.collect::<std::result::Result<_, _>>())
            .map_err(failed)?
    };
    for (tenant, session, body) in rows {
        let mut snapshot: Snapshot = serde_json::from_str(&body)
            .map_err(|error| Error::invalid(format!("stored snapshot is unreadable: {error}")))?;
        if snapshot.messages.is_empty() && snapshot.ledger.attempts.is_empty() {
            continue;
        }
        let messages = std::mem::take(&mut snapshot.messages);
        let attempts = std::mem::take(&mut snapshot.ledger.attempts);
        append_rows(&transaction, &tenant, &session, "messages", &messages, 0)?;
        append_rows(&transaction, &tenant, &session, "attempts", &attempts, 0)?;
        let body = serde_json::to_string(&snapshot)
            .map_err(|error| Error::internal(format!("snapshot is not storable: {error}")))?;
        transaction
            .execute(
                "update snapshots set body = ?1, messages_held = ?2, attempts_held = ?3
                 where tenant = ?4 and session = ?5",
                params![
                    body,
                    i64::try_from(messages.len()).unwrap_or(i64::MAX),
                    i64::try_from(attempts.len()).unwrap_or(i64::MAX),
                    tenant,
                    session
                ],
            )
            .map_err(failed)?;
    }
    // The version-2 side table is superseded: the body no longer grows with the conversation, so the
    // head and the body are the same row.
    transaction
        .execute_batch("drop table if exists checkpoint_heads")
        .map_err(failed)?;
    transaction.commit().map_err(failed)
}

/// The version a store records, if it records one.
fn read_version(connection: &Connection) -> Result<Option<u32>> {
    let stored: Option<String> = connection
        .query_row(
            "select value from meta where key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(failed)?;
    match stored {
        None => Ok(None),
        Some(value) => value.parse().map(Some).map_err(|_| {
            Error::invalid(format!(
                "session store schema version {value:?} is not a number"
            ))
        }),
    }
}

impl SqliteStore {
    /// Record the version on first open, and refuse a store written by a newer build.
    ///
    /// The refusal is the point. `sessions.db` is the durable record of every conversation; opening one
    /// written by a newer daemon with older assumptions is how events stop being written (a column that
    /// is now required), or worse, are written in a shape the new build cannot fold. "Upgrade the
    /// daemon, do not downgrade the store" is a sentence an operator can act on; a parse error at the
    /// first `load` is not.
    fn check_version(&self) -> Result<()> {
        let connection = self.pool.writer();
        let stored: Option<String> = connection
            .query_row(
                "select value from meta where key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(failed)?;
        match stored {
            None => {
                connection
                    .execute(
                        "insert into meta (key, value) values ('schema_version', ?1)",
                        params![SCHEMA_VERSION.to_string()],
                    )
                    .map_err(failed)?;
                Ok(())
            }
            Some(value) => {
                let version: u32 = value.parse().map_err(|_| {
                    Error::invalid(format!(
                        "session store schema version {value:?} is not a number"
                    ))
                })?;
                if version > SCHEMA_VERSION {
                    return Err(Error::invalid(format!(
                        "session store is schema {version}, this build understands {SCHEMA_VERSION}; \
                         upgrade nucleosd rather than downgrading the store"
                    )));
                }
                Ok(())
            }
        }
    }

    /// The version this store records.
    pub fn schema_version(&self) -> Result<u32> {
        let connection = self.pool.writer();
        let stored: Option<String> = connection
            .query_row(
                "select value from meta where key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(failed)?;
        Ok(stored
            .and_then(|value| value.parse().ok())
            .unwrap_or(SCHEMA_VERSION))
    }
}

#[cfg(test)]
mod version_and_foreign_kinds {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// An event written by a build that knows more kinds than this one.
    ///
    /// Inserted as raw JSON, the way a newer daemon would have written it. Refusing it would make the
    /// whole session unreadable over one event the reader does not recognise — the exact failure mode a
    /// durable format may not have.
    #[tokio::test]
    async fn a_kind_from_the_future_is_carried_not_refused() {
        let store = SqliteStore::in_memory().unwrap();
        let key = SessionKey::new("acme", SessionId::new("s1"));
        let body = r#"{"seq":0,"session":"s1","at_unix_millis":1,"schema_version":99,
                       "kind":"something_from_the_future","detail":{"anything":true}}"#;
        store
            .run(move |connection| {
                connection
                    .execute(
                        "insert into events (tenant, session, seq, body) values ('acme','s1',0,?1)",
                        params![body],
                    )
                    .map_err(failed)?;
                connection
                    .execute(
                        "insert into sessions (tenant, session, kind) values ('acme','s1',null)",
                        [],
                    )
                    .map_err(failed)?;
                Ok(())
            })
            .await
            .unwrap();

        let events = store.events_since(&key, 0).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, nucleos::contract::EventKind::Unknown);
        assert_eq!(events[0].schema_version, 99, "the version is carried too");
    }

    /// A store written by a newer daemon refuses to open, and says what to do about it.
    #[tokio::test]
    async fn a_store_from_a_newer_build_is_refused() {
        let path = std::env::temp_dir().join(format!("nucleos-future-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .run(|connection| {
                    connection
                        .execute(
                            "update meta set value = '99' where key = 'schema_version'",
                            [],
                        )
                        .map_err(failed)?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let error = SqliteStore::open(&path).unwrap_err();
        assert!(
            error.reason().contains("upgrade nucleosd"),
            "{}",
            error.reason()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// An older store opens: tolerating additive change is what the version is for.
    #[tokio::test]
    async fn a_store_from_an_older_build_opens() {
        let path = std::env::temp_dir().join(format!("nucleos-past-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .run(|connection| {
                    connection
                        .execute(
                            "update meta set value = '0' where key = 'schema_version'",
                            [],
                        )
                        .map_err(failed)?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        assert!(SqliteStore::open(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
