//! The SQLite store.

use async_trait::async_trait;
use nucleos::adapter::{SessionKey, SessionPage, SessionQuery, Store};
use nucleos::contract::{Event, SessionId, SessionKind, Snapshot};
use nucleos::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A store backed by one SQLite database file.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open, or create, a database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path).map_err(failed)?)
    }

    /// A database in memory.
    ///
    /// The same code path as a file, so a test against it exercises the real store rather than a
    /// second implementation that agrees with it by accident. Each call is an independent database,
    /// which is what a test wants.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(failed)?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        // WAL is ignored for in-memory databases, which is fine: there is nothing to recover.
        connection
            .pragma_update(None, "journal_mode", "wal")
            .map_err(failed)?;
        connection
            .pragma_update(None, "synchronous", "normal")
            .map_err(failed)?;
        connection
            .pragma_update(None, "foreign_keys", "on")
            .map_err(failed)?;
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
                     body    text    not null,
                     primary key (tenant, session)
                 ) without rowid;

                 -- The listing's own table: a session exists once it has an event, and its kind is
                 -- only known once a checkpoint says so. Keeping it separate from the log means a
                 -- page of ids does not read a single event body.
                 create table if not exists sessions (
                     tenant  text    not null,
                     session text    not null,
                     kind    text,
                     primary key (tenant, session)
                 ) without rowid;
                 create index if not exists sessions_by_kind on sessions (tenant, kind, session);",
            )
            .map_err(failed)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Run `work` on the blocking pool with the connection.
    async fn run<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        // A panic in the closure poisons the mutex; `into_inner` recovers the connection rather
        // than turning one bad call into a store that refuses everything afterwards.
        tokio::task::spawn_blocking(move || {
            let mut guard = connection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&mut guard)
        })
        .await
        .map_err(|error| Error::internal(format!("the store task failed: {error}")))?
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn append(&self, session: &SessionKey, events: &[Event]) -> Result<()> {
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        let events = events.to_vec();
        self.run(move |connection| {
            let transaction = connection.transaction().map_err(failed)?;
            let next: Option<i64> = transaction
                .query_row(
                    "select max(seq) + 1 from events where tenant = ?1 and session = ?2",
                    params![tenant, session.as_str()],
                    |row| row.get(0),
                )
                .map_err(failed)?;
            let mut expected = next.map_or(0, |seq| u64::try_from(seq).unwrap_or(u64::MAX));

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

            // The session exists from its first event; its kind waits for a checkpoint.
            transaction
                .execute(
                    "insert into sessions (tenant, session, kind) values (?1, ?2, null)
                     on conflict (tenant, session) do nothing",
                    params![tenant, session.as_str()],
                )
                .map_err(failed)?;
            transaction.commit().map_err(failed)
        })
        .await
    }

    async fn sessions(&self, query: &SessionQuery) -> Result<SessionPage> {
        let query = query.clone();
        self.run(move |connection| {
            // One row past the page, so "is there more" costs nothing extra.
            let limit = i64::try_from(query.limit.saturating_add(1)).unwrap_or(i64::MAX);
            let after = query.after.as_ref().map(SessionId::as_str).unwrap_or("");
            let kind = query.kind.map(kind_name);

            let mut statement = connection
                .prepare(
                    "select session from sessions
                     where tenant = ?1 and session > ?2 and (?3 is null or kind = ?3)
                     order by session limit ?4",
                )
                .map_err(failed)?;
            let rows = statement
                .query_map(params![query.tenant, after, kind, limit], |row| {
                    row.get::<_, String>(0)
                })
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
        self.run(move |connection| {
            let body: Option<String> = connection
                .query_row(
                    "select body from snapshots where tenant = ?1 and session = ?2",
                    params![tenant, session.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(failed)?;
            match body {
                Some(body) => serde_json::from_str(&body).map(Some).map_err(|error| {
                    Error::invalid(format!("stored snapshot is unreadable: {error}"))
                }),
                None => Ok(None),
            }
        })
        .await
    }

    async fn write_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        self.run(move |connection| {
            let held: i64 = connection
                .query_row(
                    "select count(*) from events where tenant = ?1 and session = ?2",
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
            let body = serde_json::to_string(&snapshot)
                .map_err(|error| Error::internal(format!("snapshot is not storable: {error}")))?;
            let transaction = connection.transaction().map_err(failed)?;
            transaction
                .execute(
                    "insert into snapshots (tenant, session, seq, body) values (?1, ?2, ?3, ?4)
                     on conflict (tenant, session) do update set seq = excluded.seq, body = excluded.body",
                    params![
                        snapshot.principal.tenant,
                        snapshot.session.as_str(),
                        i64::try_from(snapshot.seq).unwrap_or(i64::MAX),
                        body
                    ],
                )
                .map_err(failed)?;
            transaction
                .execute(
                    "insert into sessions (tenant, session, kind) values (?1, ?2, ?3)
                     on conflict (tenant, session) do update set kind = excluded.kind",
                    params![
                        snapshot.principal.tenant,
                        snapshot.session.as_str(),
                        kind_name(snapshot.kind)
                    ],
                )
                .map_err(failed)?;
            transaction.commit().map_err(failed)
        })
        .await
    }

    async fn events_since(&self, session: &SessionKey, seq: u64) -> Result<Vec<Event>> {
        let (tenant, session) = (session.tenant.clone(), session.id.clone());
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "select body from events where tenant = ?1 and session = ?2 and seq >= ?3 \
                     order by seq",
                )
                .map_err(failed)?;
            let rows = statement
                .query_map(
                    params![tenant, session.as_str(), i64::try_from(seq).unwrap_or(i64::MAX)],
                    |row| row.get::<_, String>(0),
                )
                .map_err(failed)?;
            let mut events = Vec::new();
            for body in rows {
                let body = body.map_err(failed)?;
                events.push(serde_json::from_str(&body).map_err(|error| {
                    Error::invalid(format!("stored event is unreadable: {error}"))
                })?);
            }
            Ok(events)
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
