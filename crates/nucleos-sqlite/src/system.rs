//! The system store: what the daemon is, as opposed to what it is doing.
//!
//! Two databases, two lifecycles. This one holds configuration — model definitions, tool sources,
//! policy versions, credential hashes, administrator keys — and changes a few times a day. The other
//! holds sessions, events and snapshots, and changes once per token.
//!
//! They are separate files because a database that is pruned by age and a database that is versioned
//! and kept have nothing to say to each other: one file would either never be pruned, or prune the
//! configuration with the sessions. Separating them also keeps configuration writes out of the
//! session write-ahead log, and a corrupt sessions database cannot take the model registry with it.
//!
//! The schema here is deliberately almost empty. Configuration itself is not written yet — that
//! arrives with the administrative ABI — but the file has to exist for a reason, so it holds a schema
//! version and a key-value table for everything that follows.

use crate::pool::{Pool, configure};
use nucleos::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// Set once the deployment has issued its first token, and never cleared.
pub const TOKENS_ENABLED_KEY: &str = "auth/tokens_enabled";

/// The version this build writes. A store with another version is refused rather than guessed at.
pub const SCHEMA_VERSION: u32 = 1;

/// One tenant token, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    /// What `list` shows and `remove` names.
    pub id: String,
    /// The tenant it authenticates.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
    /// SHA-256 of the token, hex. Empty when read back from `list`, which never carries it.
    pub hash: String,
    /// When it was issued.
    pub created_unix_millis: u64,
}

/// One recorded configuration change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// When it happened, unix milliseconds.
    pub at_unix_millis: u64,
    /// The tenant that made it.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
    /// Which administrator key signed it.
    pub key_id: String,
    /// What was changed: `policy`, `models`, `sources`, `tokens`, `admin_keys`.
    pub key: String,
    /// The new value, as stored.
    pub value: String,
}

/// The daemon's own configuration database.
#[derive(Debug, Clone)]
pub struct SystemStore {
    pool: std::sync::Arc<Pool>,
}

impl SystemStore {
    /// Open, or create, the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let connection = Connection::open(path).map_err(failed)?;
        configure(&connection)?;
        connection
            .execute_batch(
                "create table if not exists meta (
                     key   text primary key,
                     value text not null
                 ) without rowid;

                 -- Configuration changes, append-only. Here rather than in the session event log
                 -- because a deployment that prunes session data must not prune its own audit trail:
                 -- that is the reason the two databases are separate at all.
                 -- Tenant tokens, stored hashed. A row here is what makes a caller a tenant; there
                 -- is no other way in, and no environment variable that could disagree.
                 create table if not exists tokens (
                     id      text primary key,
                     tenant  text not null,
                     subject text not null,
                     hash    text not null unique,
                     created integer not null
                 ) without rowid;

                 create table if not exists history (
                     at_unix_millis integer not null,
                     subject        text    not null,
                     tenant         text    not null,
                     key_id         text    not null,
                     key            text    not null,
                     value          text    not null
                 );",
            )
            .map_err(failed)?;

        let store = Self {
            pool: std::sync::Arc::new(if path == Path::new(":memory:") {
                Pool::single(connection)
            } else {
                Pool::file(connection, path, |path| {
                    let reader = Connection::open(path).map_err(failed)?;
                    configure(&reader)?;
                    Ok(reader)
                })?
            }),
        };
        store.check_version()?;
        Ok(store)
    }

    /// An empty store in memory, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// The schema version, written on first open.
    pub async fn schema_version(&self) -> Result<u32> {
        let read = self.get("schema_version").await?;
        Ok(read
            .and_then(|value| value.parse().ok())
            .unwrap_or(SCHEMA_VERSION))
    }

    /// A configuration value, by key.
    ///
    /// Async, and on a reader: this is called on every request to decide whether a token is required,
    /// and a synchronous read would hold the writer's lock on the reactor thread while a checkpoint
    /// of somebody else's session is being written.
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        self.pool
            .read(move |connection| {
                connection
                    .query_row(
                        "select value from meta where key = ?1",
                        params![key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(failed)
            })
            .await
    }

    /// Set a configuration value.
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.run(move |connection| {
            connection
                .execute(
                    "insert into meta (key, value) values (?1, ?2)
                     on conflict (key) do update set value = excluded.value",
                    params![key, value],
                )
                .map_err(failed)?;
            Ok(())
        })
        .await
    }

    /// Append one configuration change to the history.
    ///
    /// Append-only and never rewritten: what a deployment enforced last Tuesday is a fact about it,
    /// and a record that can be edited answers no question worth asking.
    pub async fn record(&self, change: Change) -> Result<()> {
        self.run(move |connection| {
            connection
                .execute(
                    "insert into history (at_unix_millis, subject, tenant, key_id, key, value)
                     values (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        i64::try_from(change.at_unix_millis).unwrap_or(i64::MAX),
                        change.subject,
                        change.tenant,
                        change.key_id,
                        change.key,
                        change.value
                    ],
                )
                .map_err(failed)?;
            Ok(())
        })
        .await
    }

    /// Replace the whole configuration namespace in one transaction.
    ///
    /// One transaction because a configuration is one thing: a half-applied document is a deployment
    /// that refers to a model nobody defined, and the window between two writes is exactly when the
    /// daemon is asked to serve a request.
    ///
    /// What is **not** in the namespace, and survives the replace: the sealed model credentials
    /// (`config/keys/*`), the revision counter, the history, the recovery password, the token table,
    /// and the schema version. Credentials are not configuration — exporting ciphertext sealed with a
    /// key that lives outside the data directory would be an export nobody can restore.
    pub async fn replace(&self, entries: Vec<(String, String)>) -> Result<()> {
        self.run(move |connection| {
            let transaction = connection.transaction().map_err(failed)?;
            transaction
                .execute(
                    "delete from meta
                     where (key like 'config/%'
                            and key not like 'config/keys/%'
                            and key <> 'config/revision'
                            and key not like 'config/history/%')",
                    [],
                )
                .map_err(failed)?;
            for (key, value) in &entries {
                transaction
                    .execute(
                        "insert into meta (key, value) values (?1, ?2)
                         on conflict (key) do update set value = excluded.value",
                        params![key, value],
                    )
                    .map_err(failed)?;
            }
            transaction.commit().map_err(failed)
        })
        .await
    }

    /// The configuration revision: how many times a document has been applied.
    pub async fn revision(&self) -> Result<u64> {
        Ok(self
            .get("config/revision")
            .await?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    /// Every entry whose key starts with `prefix`, in key order.
    ///
    /// Used for scoped configuration — `config/policy/tenant/` — where the set of tenants is whatever
    /// has been written rather than a list somebody has to keep in step.
    pub async fn with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let prefix = prefix.to_owned();
        self.pool
            .read(move |connection| {
                let mut statement = connection
                    .prepare(
                        "select key, value from meta where key >= ?1 and key < ?2 order by key",
                    )
                    .map_err(failed)?;
                // `>= prefix` and `< prefix` with the last byte incremented: a range scan on the
                // primary key rather than a `like` that cannot use it.
                let mut upper = prefix.clone();
                upper.push(char::MAX);
                let rows = statement
                    .query_map(params![prefix, upper], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(failed)?;
                rows.collect::<std::result::Result<_, _>>().map_err(failed)
            })
            .await
    }

    /// Record an issued token. The hash, never the token.
    pub async fn add_token(&self, token: &IssuedToken) -> Result<()> {
        let token = token.clone();
        self.run(move |connection| {
            connection
                .execute(
                    "insert into tokens (id, tenant, subject, hash, created) values (?1, ?2, ?3, ?4, ?5)",
                    params![
                        token.id,
                        token.tenant,
                        token.subject,
                        token.hash,
                        i64::try_from(token.created_unix_millis).unwrap_or(i64::MAX)
                    ],
                )
                .map_err(failed)?;
            Ok(())
        })
        .await
    }

    /// The tenant a presented token belongs to, if it is one of ours.
    ///
    /// Read on every request rather than cached: revocation has to be immediate, and a token that
    /// keeps working for the length of a cache is a token somebody revoked.
    pub async fn tenant_for_hash(&self, hash: &str) -> Result<Option<(String, String, String)>> {
        let hash = hash.to_owned();
        self.pool
            .read(move |connection| {
                connection
                    .query_row(
                        "select id, tenant, subject from tokens where hash = ?1",
                        params![hash],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(failed)
            })
            .await
    }

    /// Whether any token exists right now.
    pub async fn has_tokens(&self) -> Result<bool> {
        self.pool
            .read(|connection| {
                let count: i64 = connection
                    .query_row("select count(*) from tokens", [], |row| row.get(0))
                    .map_err(failed)?;
                Ok(count > 0)
            })
            .await
    }

    /// Whether this deployment has ever issued a token.
    ///
    /// Sticky, and separate from [`Self::has_tokens`] on purpose: revoking the last token must not put the
    /// daemon back into the mode where the socket's permissions are the whole authentication. A
    /// deployment that once had tenants still has them — they are just all revoked, which is what
    /// "no valid token" has to mean once the door has been opened.
    pub async fn tokens_required(&self) -> Result<bool> {
        Ok(self.get(TOKENS_ENABLED_KEY).await?.is_some() || self.has_tokens().await?)
    }

    /// Record that this deployment issues tokens. Never cleared.
    pub async fn require_tokens(&self) -> Result<()> {
        if self.get(TOKENS_ENABLED_KEY).await?.is_none() {
            self.set(TOKENS_ENABLED_KEY, "true").await?;
        }
        Ok(())
    }

    /// Every token, without the hash.
    pub async fn list_tokens(&self) -> Result<Vec<IssuedToken>> {
        self.pool
            .read(|connection| {
                let mut statement = connection
                    .prepare("select id, tenant, subject, created from tokens order by created, id")
                    .map_err(failed)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok(IssuedToken {
                            id: row.get(0)?,
                            tenant: row.get(1)?,
                            subject: row.get(2)?,
                            hash: String::new(),
                            created_unix_millis: row.get::<_, i64>(3)? as u64,
                        })
                    })
                    .map_err(failed)?;
                rows.collect::<std::result::Result<_, _>>().map_err(failed)
            })
            .await
    }

    /// Forget one. `true` if it was there.
    pub async fn revoke_token(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.run(move |connection| {
            let removed = connection
                .execute("delete from tokens where id = ?1", params![id])
                .map_err(failed)?;
            Ok(removed > 0)
        })
        .await
    }

    /// The history, newest last.
    pub async fn history(&self, limit: usize) -> Result<Vec<Change>> {
        self.pool
            .read(move |connection| {
                let mut statement = connection
                    .prepare(
                        // Newest first: `limit` means "how many do I want to know about", and the
                        // answer to that is the recent ones. The old order made `--limit 3` a list of
                        // the three oldest changes a deployment ever had, which is never the question.
                        "select at_unix_millis, tenant, subject, key_id, key, value
                         from history order by at_unix_millis desc, rowid desc limit ?1",
                    )
                    .map_err(failed)?;
                let rows = statement
                    .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                        Ok(Change {
                            at_unix_millis: row.get::<_, i64>(0)? as u64,
                            tenant: row.get(1)?,
                            subject: row.get(2)?,
                            key_id: row.get(3)?,
                            key: row.get(4)?,
                            value: row.get(5)?,
                        })
                    })
                    .map_err(failed)?;
                rows.collect::<std::result::Result<_, _>>().map_err(failed)
            })
            .await
    }

    fn check_version(&self) -> Result<()> {
        // Synchronous on purpose: this runs at open, before the store is shared, so there is no
        // reactor to stall and no reader to wait for.
        let stored = self
            .pool
            .writer()
            .query_row(
                "select value from meta where key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(failed)?;
        match stored {
            None => {
                // First open: record the version this build understands.
                let connection = self.pool.writer();
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
                        "system store schema version {value:?} is not a number"
                    ))
                })?;
                if version > SCHEMA_VERSION {
                    // Refused rather than guessed: a newer daemon wrote this, and opening it with
                    // older assumptions is how configuration quietly disappears.
                    return Err(Error::invalid(format!(
                        "system store is schema {version}, this build understands {SCHEMA_VERSION}; \
                         upgrade nucleosd rather than downgrading the store"
                    )));
                }
                Ok(())
            }
        }
    }

    async fn run<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.pool.write(work).await
    }
}

/// A driver error, turned into ours and said out loud.
fn failed(error: rusqlite::Error) -> Error {
    tracing::error!(event = "system_store_failed", detail = %error);
    Error::internal(format!("system store: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn the_first_open_records_the_schema_version() {
        let store = SystemStore::in_memory().unwrap();
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn values_round_trip_and_overwrite() {
        let store = SystemStore::in_memory().unwrap();
        assert_eq!(store.get("models").await.unwrap(), None);
        store.set("models", "[]").await.unwrap();
        assert_eq!(store.get("models").await.unwrap().as_deref(), Some("[]"));
        store.set("models", "[1]").await.unwrap();
        assert_eq!(store.get("models").await.unwrap().as_deref(), Some("[1]"));
    }

    #[tokio::test]
    async fn a_newer_schema_is_refused_rather_than_guessed() {
        let path = std::env::temp_dir().join(format!("nucleos-sys-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let store = SystemStore::open(&path).unwrap();
            store.get("schema_version").await.unwrap();
            let connection = store.pool.writer();
            connection
                .execute(
                    "update meta set value = '99' where key = 'schema_version'",
                    [],
                )
                .unwrap();
        }

        let error = SystemStore::open(&path).expect_err("a newer store is refused");
        assert!(error.reason().contains("upgrade nucleosd"), "{error:?}");
        let _ = std::fs::remove_file(&path);
    }
}
