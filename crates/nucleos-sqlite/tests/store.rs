//! The store's own rules, and the one property it exists for: a session survives the process.
//!
//! The rules are the ones the kernel relies on and cannot check itself — a gap, a rewrite, a
//! snapshot that claims more than the log holds. The property is the one a deployment notices:
//! close everything, reopen the file, and the session continues where it stopped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{SessionKey, SessionQuery, Store};
use nucleos::contract::{Event, EventKind, SessionId, SessionKind, SessionState};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FakeTool, FixedClock};
use std::sync::Arc;

/// The store's key for a test tenant. The tests all use one tenant, so the helper keeps them about
/// what they are testing rather than about spelling out a key at every call.
fn key(id: &SessionId) -> nucleos::adapter::SessionKey {
    nucleos::adapter::SessionKey::new("acme", id.clone())
}

fn event(session: &SessionId, seq: u64) -> Event {
    Event {
        seq,
        session: session.clone(),
        at_unix_millis: 1_000,
        schema_version: 1,
        kind: EventKind::StateChanged {
            state: SessionState::Open,
            stop: None,
        },
        meta: std::collections::BTreeMap::new(),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("nucleos-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn append_refuses_a_gap_rather_than_renumbering() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();

    // A silently renumbered event makes every later resume cursor wrong, so this is refused.
    let gap = store.append(&key(&session), &[event(&session, 5)]).await;
    assert!(gap.is_err(), "{gap:?}");

    // And the log is unchanged: a refused batch appends nothing at all.
    assert_eq!(
        store.events_since(&key(&session), 0).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn append_refuses_an_event_for_another_session() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    let other = SessionId::new("s2");
    // Events from one session written into another's log is a corruption no later read can undo.
    assert!(
        store
            .append(&key(&session), &[event(&other, 0)])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn events_since_is_inclusive() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    let events: Vec<Event> = (0..5).map(|seq| event(&session, seq)).collect();
    store.append(&key(&session), &events).await.unwrap();

    // A client asking from `seq` wants that event, not the one after it.
    let from_two: Vec<u64> = store
        .events_since(&key(&session), 2)
        .await
        .unwrap()
        .iter()
        .map(|event| event.seq)
        .collect();
    assert_eq!(from_two, [2, 3, 4]);
}

#[tokio::test]
async fn a_snapshot_may_not_claim_events_the_log_does_not_hold() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();

    let mut snapshot = snapshot_for(&session);
    snapshot.seq = 7;
    // Recovery would load this and then ask for everything after 7, skipping what exists.
    assert!(store.write_snapshot(snapshot).await.is_err());
}

#[tokio::test]
async fn a_listing_pages_by_cursor_and_filters_by_kind() {
    let store = SqliteStore::in_memory().unwrap();
    for name in ["s1", "s2", "s3"] {
        let session = SessionId::new(name);
        store
            .append(&key(&session), &[event(&session, 0)])
            .await
            .unwrap();
    }
    store
        .write_snapshot({
            let mut snapshot = snapshot_for(&SessionId::new("s2"));
            snapshot.seq = 0;
            snapshot
        })
        .await
        .unwrap();

    let first = store
        .sessions(&SessionQuery {
            tenant: "acme".to_owned(),
            subject: None,
            kind: None,
            limit: 2,
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.sessions.len(), 2);
    let next = first.next.expect("there is a third session");

    let second = store
        .sessions(&SessionQuery {
            tenant: "acme".to_owned(),
            subject: None,
            kind: None,
            limit: 2,
            after: Some(next),
        })
        .await
        .unwrap();
    assert_eq!(
        second.sessions.len(),
        1,
        "the cursor resumes after the page"
    );

    // Only the checkpointed session has a kind yet, which is what the column is for.
    let processes = store
        .sessions(&SessionQuery {
            tenant: "acme".to_owned(),
            subject: None,
            kind: Some(SessionKind::Process),
            limit: 10,
            after: None,
        })
        .await
        .unwrap();
    assert!(processes.sessions.is_empty(), "s2 is an agent session");
}

#[tokio::test]
async fn a_session_survives_the_process_that_wrote_it() {
    let directory = temp_dir("durable");
    let path = directory.join("nucleos.db");

    // A whole turn: a model answer, so the log has events, and a checkpoint.
    let session = {
        let kernel = nucleos::Kernel::builder()
            .model("primary", Arc::new(FakeModel::text(["hello"])))
            .tool(Arc::new(FakeTool::new("echo", "hi")))
            .store(Arc::new(SqliteStore::open(&path).unwrap()))
            .policy(Arc::new(nucleos::adapter::AllowAll))
            .clock(Arc::new(FixedClock::at(0)))
            .routing(nucleos::contract::Routing {
                chain: vec!["primary".to_owned()],
                retries: 0,
            })
            .build()
            .unwrap();
        let session = kernel
            .open(
                &nucleos::contract::Principal {
                    tenant: "acme".to_owned(),
                    subject: "alice".to_owned(),
                },
                SessionId::new("s1"),
                nucleos::kernel::SessionSpec {
                    // The store test is about durability, not about the budget gate.
                    credit_tokens: Some(1_000_000),
                    ..nucleos::kernel::SessionSpec::default()
                },
            )
            .await
            .unwrap();
        let mut session = session;
        let outcome = kernel.inference(&mut session, &[]).await.unwrap();
        assert_eq!(outcome.finish, nucleos::adapter::FinishReason::Stop);
        session
    };

    // Nothing of the first process is left: a new store on the same file, a new kernel.
    let reopened = SqliteStore::open(&path).unwrap();
    let snapshot = reopened
        .snapshot(&key(session.id()))
        .await
        .unwrap()
        .expect("the checkpoint was written");
    // The checkpoint may be behind the log — that is the throttle, and `load` folds the tail — but it
    // can never claim an event the log does not hold.
    assert!(
        snapshot.seq < session.next_seq(),
        "a checkpoint at {} claims the log's end at {}",
        snapshot.seq,
        session.next_seq()
    );

    let kernel = nucleos::Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["again"])))
        .store(Arc::new(reopened))
        .policy(Arc::new(nucleos::adapter::AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(nucleos::contract::Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    let loaded = kernel
        .load(
            &nucleos::contract::Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            &SessionId::new("s1"),
        )
        .await
        .unwrap();
    assert_eq!(loaded.id(), session.id());
    assert_eq!(
        loaded.messages().len(),
        1,
        "the tail the checkpoint did not cover was folded: the turn's message is the recovery record"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A checkpoint with no content, for the tests that are about the log rather than about a turn.
fn snapshot_for(session: &SessionId) -> nucleos::contract::Snapshot {
    nucleos::contract::Snapshot {
        principal: nucleos::contract::Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        },
        session: session.clone(),
        seq: 0,
        version: 1,
        generation: nucleos::contract::Generation(0),
        state: SessionState::Open,
        messages: Vec::new(),
        usage: nucleos::contract::Usage::default(),
        ledger: Default::default(),
        inference_calls: 0,
        credit_tokens: Some(0),
        budget: Default::default(),
        spent_wall_millis: 0,
        pending: Vec::new(),
        data: std::collections::BTreeMap::new(),
        routing: nucleos::contract::Routing::default(),
        system: None,
        tools: Vec::new(),
        params: nucleos::contract::GenerateParams::default(),
        workspace: None,
        sandbox: nucleos::contract::SandboxPolicy::default(),
        hints: nucleos::contract::Hints::default(),
        tool_policy: nucleos::contract::ToolPolicy::default(),
        discovered: std::collections::BTreeMap::new(),
        round: 0,
        stop: None,
        kind: SessionKind::Agent,
        process: None,
    }
}

#[tokio::test]
async fn two_tenants_can_share_a_session_id() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");

    // The same identifier in two tenants: ordinary, not adversarial. Callers choose their ids.
    for tenant in ["acme", "other"] {
        let key = nucleos::adapter::SessionKey::new(tenant, session.clone());
        store.append(&key, &[event(&session, 0)]).await.unwrap();
    }

    // Each key sees exactly its own log, and the entries did not collide: both writes succeeded,
    // so both rows exist under different keys.
    for tenant in ["acme", "other"] {
        let key = nucleos::adapter::SessionKey::new(tenant, session.clone());
        assert_eq!(store.events_since(&key, 0).await.unwrap().len(), 1);
    }

    // And a listing is scoped the same way: two rows, one per tenant.
    for tenant in ["acme", "other"] {
        let page = store
            .sessions(&SessionQuery {
                tenant: tenant.to_owned(),
                subject: None,
                kind: None,
                limit: 10,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(
            page.sessions.as_slice(),
            std::slice::from_ref(&session),
            "{tenant}"
        );
    }

    // A tenant that never wrote anything sees nothing, rather than everybody else's rows.
    let stranger = store
        .sessions(&SessionQuery {
            tenant: "stranger".to_owned(),
            subject: None,
            kind: None,
            limit: 10,
            after: None,
        })
        .await
        .unwrap();
    assert!(stranger.sessions.is_empty());
}

/// A checkpoint that is not strictly newer is refused, and changes nothing.
///
/// This is what makes a stale handle detectable: two requests load the same session, both write down a
/// materialisation of the log, and without the version the second one silently replaces the first one's
/// work with a view that never saw it.
#[tokio::test]
async fn a_checkpoint_that_is_not_newer_is_refused() {
    let store = SqliteStore::in_memory().unwrap();
    let id = SessionId::new("s1");
    let key = SessionKey::new("acme", id.clone());

    // A snapshot may not claim more of the log than exists: one event, then a checkpoint that covers it.
    store
        .append(
            &key,
            &[nucleos::contract::Event {
                seq: 0,
                session: id.clone(),
                at_unix_millis: 0,
                schema_version: nucleos::contract::Event::SCHEMA_VERSION,
                kind: nucleos::contract::EventKind::StateChanged {
                    state: nucleos::contract::SessionState::Open,
                    stop: None,
                },
                meta: Default::default(),
            }],
        )
        .await
        .unwrap();

    let mut snapshot = snapshot_for(&id);
    snapshot.seq = 0;
    snapshot.version = 2;
    store.write_snapshot(snapshot.clone()).await.unwrap();

    // The same version again: refused, and the stored body is untouched.
    let error = store.write_snapshot(snapshot.clone()).await.unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Conflict);
    assert!(error.reason().contains("not newer"), "{}", error.reason());
    assert_eq!(store.snapshot(&key).await.unwrap().unwrap().version, 2);

    // An older one too — the case a handle loaded before somebody else's checkpoint produces.
    let mut stale = snapshot;
    stale.version = 1;
    stale.state = nucleos::contract::SessionState::Closed;
    assert_eq!(
        store.write_snapshot(stale).await.unwrap_err().code(),
        nucleos::contract::ErrorCode::Conflict
    );
    assert_eq!(
        store.snapshot(&key).await.unwrap().unwrap().state,
        nucleos::contract::SessionState::Open,
        "a refused write changed nothing"
    );
}

/// One message, for the tests that care about the conversation rather than its content.
fn message() -> nucleos::contract::Message {
    nucleos::contract::Message {
        role: nucleos::contract::Role::Assistant,
        content: nucleos::contract::Content::new([nucleos::contract::Part::new(
            nucleos::contract::PartKind::Text(nucleos::contract::TextPart {
                text: "the answer".to_owned(),
            }),
        )]),
    }
}

/// The checkpoint keeps the conversation and the state in separate columns, so a view reads one
/// without parsing the other.
#[tokio::test]
async fn a_checkpoint_keeps_a_head_without_the_conversation() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();

    let mut snapshot = snapshot_for(&session);
    snapshot.messages = vec![message()];
    snapshot.seq = 0;
    snapshot.version = 4;
    store.write_snapshot(snapshot).await.unwrap();

    let full = store.snapshot(&key(&session)).await.unwrap().unwrap();
    assert_eq!(full.messages.len(), 1, "the conversation is kept");
    assert_eq!(full.version, 4);

    let head = store.snapshot_head(&key(&session)).await.unwrap().unwrap();
    assert!(head.messages.is_empty(), "the head never carries one");
    assert!(head.ledger.attempts.is_empty(), "nor the ledger");
    assert_eq!(head.state, full.state);
    assert_eq!(head.seq, full.seq);
    assert_eq!(head.version, full.version, "the column is authoritative");
}

/// A database written before the checkpoint's `version`/`head` columns still reads, and still refuses
/// a stale write.
///
/// The version is backfilled from the body once, at open: a stored `0` is not a version this code
/// writes (the first checkpoint is 1), and leaving it there would let a handle that read the session
/// long ago overwrite a newer snapshot.
#[tokio::test]
async fn a_checkpoint_written_before_the_new_columns_still_reads() {
    let dir = temp_dir("old-snapshot");
    let path = dir.join("sessions.db");
    let session = SessionId::new("s1");
    let mut snapshot = snapshot_for(&session);
    snapshot.messages = vec![message()];
    snapshot.seq = 0;
    snapshot.version = 3;
    let body = serde_json::to_string(&snapshot).unwrap();

    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "create table events (
                     tenant text not null, session text not null, seq integer not null, body text not null,
                     primary key (tenant, session, seq)
                 ) without rowid;
                 create table snapshots (
                     tenant text not null, session text not null, seq integer not null, body text not null,
                     primary key (tenant, session)
                 ) without rowid;
                 create table sessions (
                     tenant text not null, session text not null, kind text, subject text,
                     primary key (tenant, session)
                 ) without rowid;",
            )
            .unwrap();
        connection
            .execute(
                "insert into events (tenant, session, seq, body) values ('acme', 's1', 0, '{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "insert into snapshots (tenant, session, seq, body) values ('acme', 's1', 0, ?1)",
                [&body],
            )
            .unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();

    let head = store.snapshot_head(&key(&session)).await.unwrap().unwrap();
    assert!(
        head.messages.is_empty(),
        "the fallback answers from the body without it"
    );
    assert_eq!(head.version, 3, "the version was backfilled from the body");

    // And the compare-and-swap still sees the backfilled version: 3 is not newer than 3.
    let error = store.write_snapshot(snapshot).await.unwrap_err();
    assert_eq!(
        error.code(),
        nucleos::contract::ErrorCode::Conflict,
        "{}",
        error.reason()
    );
}

/// One attempt, for the tests that care about the ledger's shape.
fn attempt(index: u32) -> nucleos::contract::Attempt {
    nucleos::contract::Attempt {
        index,
        model: "primary".to_owned(),
        phase: nucleos::contract::AttemptPhase::Execute,
        trigger: nucleos::contract::AttemptTrigger::Selection,
        outcome: nucleos::contract::AttemptOutcome::Ok,
        usage: nucleos::contract::Usage::default(),
        duration_millis: 0,
        degraded: Vec::new(),
    }
}

/// A checkpoint writes what changed, not what it has.
///
/// The rows are counted through a second connection rather than through `snapshot`: reading the session
/// back would agree with a store that rewrote every message each time, which is the cost this shape
/// exists to remove.
#[tokio::test]
async fn a_checkpoint_appends_only_what_changed() {
    let dir = temp_dir("incremental");
    let path = dir.join("sessions.db");
    let store = SqliteStore::open(&path).unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();

    let mut first = snapshot_for(&session);
    first.seq = 0;
    first.version = 1;
    first.messages = vec![message()];
    first.ledger.attempts = vec![attempt(0)];
    store.write_snapshot(first).await.unwrap();

    let mut second = snapshot_for(&session);
    second.seq = 0;
    second.version = 2;
    second.messages = vec![message(), message()];
    second.ledger.attempts = vec![attempt(0), attempt(1)];
    store.write_snapshot(second).await.unwrap();

    let loaded = store.snapshot(&key(&session)).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2, "both messages read back");
    assert_eq!(loaded.ledger.attempts.len(), 2, "both attempts read back");

    let raw = rusqlite::Connection::open(&path).unwrap();
    for table in ["messages", "attempts"] {
        let rows: i64 = raw
            .query_row(
                &format!("select count(*) from {table} where session = 's1'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 2,
            "{table}: the checkpoint rewrote what it already had"
        );
    }
    let held: (i64, i64) = raw
        .query_row(
            "select messages_held, attempts_held from snapshots where session = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        held,
        (2, 2),
        "the checkpoint records how far it has written"
    );
    drop(raw);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A checkpoint may add to a conversation, never remove from one.
#[tokio::test]
async fn a_checkpoint_cannot_remove_messages() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();

    let mut three = snapshot_for(&session);
    three.seq = 0;
    three.version = 1;
    three.messages = vec![message(), message(), message()];
    store.write_snapshot(three).await.unwrap();

    let mut one = snapshot_for(&session);
    one.seq = 0;
    one.version = 2;
    one.messages = vec![message()];
    let error = store.write_snapshot(one).await.unwrap_err();
    assert_eq!(
        error.code(),
        nucleos::contract::ErrorCode::Invalid,
        "{}",
        error.reason()
    );
    assert!(
        error.reason().contains("never remove"),
        "{}",
        error.reason()
    );

    assert_eq!(
        store
            .snapshot(&key(&session))
            .await
            .unwrap()
            .unwrap()
            .messages
            .len(),
        3,
        "a refused checkpoint changed nothing"
    );
}

/// Readers and the writer proceed together, and a reader never sees half a checkpoint.
///
/// The point of the pool: one connection behind a mutex made every read wait behind every write, and
/// WAL only removes that wait if the readers have connections of their own. This drives reads and
/// writes at the same file concurrently and asserts two things — nothing fails with `SQLITE_BUSY`, and
/// every snapshot read is internally consistent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_and_the_writer_proceed_together() {
    let dir = temp_dir("pool");
    let path = dir.join("sessions.db");
    let store = std::sync::Arc::new(SqliteStore::open(&path).unwrap());
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();
    // One checkpoint before the readers start, so "no snapshot yet" is not a race this test is about.
    let mut first = snapshot_for(&session);
    first.seq = 0;
    first.version = 1;
    first.messages = vec![message()];
    first.ledger.attempts = vec![attempt(0)];
    store.write_snapshot(first).await.unwrap();

    let writer = {
        let store = std::sync::Arc::clone(&store);
        let session = session.clone();
        tokio::spawn(async move {
            for version in 2..=40u64 {
                let mut snapshot = snapshot_for(&session);
                snapshot.seq = 0;
                snapshot.version = version;
                snapshot.messages = (0..version).map(|_| message()).collect();
                snapshot.ledger.attempts = (0..version as u32).map(attempt).collect();
                store.write_snapshot(snapshot).await.unwrap();
                tokio::task::yield_now().await;
            }
        })
    };

    let mut readers = Vec::new();
    for _ in 0..8 {
        let store = std::sync::Arc::clone(&store);
        let session = session.clone();
        readers.push(tokio::spawn(async move {
            for _ in 0..40 {
                let snapshot = store
                    .snapshot(&key(&session))
                    .await
                    .unwrap()
                    .expect("a checkpoint exists after the first write");
                // The invariant a split checkpoint must keep: the held counts and the rows agree, so
                // no reader ever sees a body that claims more than was written.
                assert_eq!(
                    snapshot.messages.len() as u64,
                    snapshot.version,
                    "a reader saw a body from one checkpoint and rows from another"
                );
                assert_eq!(snapshot.ledger.attempts.len() as u64, snapshot.version);
                let head = store.snapshot_head(&key(&session)).await.unwrap().unwrap();
                assert!(head.messages.is_empty());
                tokio::task::yield_now().await;
            }
        }));
    }

    writer.await.unwrap();
    for reader in readers {
        reader.await.unwrap();
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A closed session whose last activity is old is pruned; an open one of the same age is not.
#[tokio::test]
async fn retention_removes_only_closed_sessions_that_are_old() {
    let store = SqliteStore::in_memory().unwrap();
    let closed = SessionId::new("closed");
    let open = SessionId::new("open");
    let recent = SessionId::new("recent");

    for (id, at) in [(&closed, 1_000u64), (&open, 1_000), (&recent, 9_000)] {
        store
            .append(
                &key(id),
                &[nucleos::contract::Event {
                    seq: 0,
                    session: id.clone(),
                    at_unix_millis: at,
                    schema_version: 1,
                    kind: EventKind::StateChanged {
                        state: SessionState::Open,
                        stop: None,
                    },
                    meta: Default::default(),
                }],
            )
            .await
            .unwrap();
    }
    for (id, state) in [
        (&closed, SessionState::Closed),
        (&open, SessionState::Open),
        (&recent, SessionState::Closed),
    ] {
        let mut snapshot = snapshot_for(id);
        snapshot.seq = 0;
        snapshot.version = 1;
        snapshot.state = state;
        snapshot.messages = vec![message()];
        store.write_snapshot(snapshot).await.unwrap();
    }

    // Dry run first: it reports, and removes nothing.
    let query = nucleos::adapter::PruneQuery {
        tenant: None,
        before_unix_millis: 5_000,
        limit: 100,
        dry_run: true,
    };
    let report = store.prune_closed(&query).await.unwrap();
    assert_eq!(
        report.sessions, 1,
        "only the old closed session: {report:?}"
    );
    assert_eq!(report.events, 1);
    assert_eq!(report.candidates[0].id, closed);
    assert!(
        store.snapshot(&key(&closed)).await.unwrap().is_some(),
        "a dry run deleted something"
    );

    // For real: the old closed session goes, the open one and the recent one stay.
    let report = store
        .prune_closed(&nucleos::adapter::PruneQuery {
            dry_run: false,
            ..query
        })
        .await
        .unwrap();
    assert_eq!(report.sessions, 1);
    assert!(store.snapshot(&key(&closed)).await.unwrap().is_none());
    assert!(store.snapshot(&key(&open)).await.unwrap().is_some());
    assert!(store.snapshot(&key(&recent)).await.unwrap().is_some());
    assert!(
        store
            .events_since(&key(&closed), 0)
            .await
            .unwrap()
            .is_empty(),
        "the log went with the session"
    );
}

/// A session whose activity is unknown is kept: retention deletes what it can date, not what it cannot.
#[tokio::test]
async fn retention_keeps_a_session_it_cannot_date() {
    let dir = temp_dir("retention-unknown");
    let path = dir.join("sessions.db");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "create table events (
                     tenant text not null, session text not null, seq integer not null, body text not null,
                     primary key (tenant, session, seq)
                 ) without rowid;
                 create table snapshots (
                     tenant text not null, session text not null, seq integer not null, body text not null,
                     primary key (tenant, session)
                 ) without rowid;
                 create table sessions (
                     tenant text not null, session text not null, kind text, subject text,
                     primary key (tenant, session)
                 ) without rowid;
                 insert into sessions (tenant, session, kind) values ('acme', 'old', null);",
            )
            .unwrap();
        // A closed checkpoint, so the row is only excluded because its activity is unknown.
        let mut snapshot = snapshot_for(&SessionId::new("old"));
        snapshot.state = SessionState::Closed;
        let body = serde_json::to_string(&snapshot).unwrap();
        connection
            .execute(
                "insert into snapshots (tenant, session, seq, body) values ('acme', 'old', 0, ?1)",
                [&body],
            )
            .unwrap();
    }
    let store = SqliteStore::open(&path).unwrap();
    let report = store
        .prune_closed(&nucleos::adapter::PruneQuery {
            tenant: None,
            before_unix_millis: u64::MAX,
            limit: 100,
            dry_run: true,
        })
        .await
        .unwrap();
    assert_eq!(report.sessions, 0, "an undated session is never pruned");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Closing is recorded on its own, so a refused checkpoint cannot leave a session unremovable.
///
/// `closed` is what `DELETE` and retention consult. It used to travel only inside a snapshot, so a
/// checkpoint refused as stale left the store believing a closed session was open — and `close` never
/// retries, so nothing ever corrected it.
#[tokio::test]
async fn closing_is_recorded_without_a_checkpoint() {
    let store = SqliteStore::in_memory().unwrap();
    let session = SessionId::new("s1");
    store
        .append(&key(&session), &[event(&session, 0)])
        .await
        .unwrap();
    // No snapshot at all: the session exists only in the log and the listing table.
    assert!(store.snapshot(&key(&session)).await.unwrap().is_none());

    store.mark_closed(&key(&session), 5_000).await.unwrap();

    // Retention can now see it, which is the property that matters.
    let report = store
        .prune_closed(&nucleos::adapter::PruneQuery {
            tenant: None,
            before_unix_millis: 9_000,
            limit: 10,
            dry_run: true,
        })
        .await
        .unwrap();
    assert_eq!(
        report.sessions, 1,
        "a closed session without a snapshot is prunable"
    );
}
