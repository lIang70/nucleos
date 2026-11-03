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
                    credit_tokens: 1_000_000,
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
        credit_tokens: 0,
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
