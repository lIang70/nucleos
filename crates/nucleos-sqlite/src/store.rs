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
                     subject text,
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
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
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

            // Strictly newer, or nothing. Equal versions mean two handles loaded the same snapshot and
            // both think they are the next writer: the first is right and the second is working from a
            // log it read before the first one's work existed. Accepting it would overwrite that work
            // with a materialisation that never saw it.
            let stored: Option<i64> = transaction
                .query_row(
                    "select json_extract(body, '$.version') from snapshots
                     where tenant = ?1 and session = ?2",
                    params![snapshot.principal.tenant, snapshot.session.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(failed)?;
            let stored = stored.map_or(0, |version| u64::try_from(version).unwrap_or(u64::MAX));
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
                    "insert into sessions (tenant, session, kind, subject) values (?1, ?2, ?3, ?4)
                     on conflict (tenant, session) do update set kind = excluded.kind,
                                                                 subject = excluded.subject",
                    params![
                        snapshot.principal.tenant,
                        snapshot.session.as_str(),
                        kind_name(snapshot.kind),
                        snapshot.principal.subject.clone(),
                    ],
                )
                .map_err(failed)?;
            transaction.commit().map_err(failed)
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
            for table in ["events", "snapshots", "sessions"] {
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
                    params![
                        tenant,
                        session.as_str(),
                        i64::try_from(seq).unwrap_or(i64::MAX)
                    ],
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

/// The session store's schema version.
pub const SCHEMA_VERSION: u32 = 1;

impl SqliteStore {
    /// Record the version on first open, and refuse a store written by a newer build.
    ///
    /// The refusal is the point. `sessions.db` is the durable record of every conversation; opening one
    /// written by a newer daemon with older assumptions is how events stop being written (a column that
    /// is now required), or worse, are written in a shape the new build cannot fold. "Upgrade the
    /// daemon, do not downgrade the store" is a sentence an operator can act on; a parse error at the
    /// first `load` is not.
    fn check_version(&self) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
