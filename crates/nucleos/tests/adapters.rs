//! The five contracts, exercised through their fakes.
//!
//! These are not tests of the fakes; they are tests of the *rules the traits state*, using
//! the fakes as the cheapest way to state them. When a real adapter disagrees with one of
//! these, the trait was wrong, not the test.

#![allow(clippy::unwrap_used)]

use nucleos::Result;
use nucleos::adapter::{
    AllowAll, Clock, DenyAll, FakeModel, FakeTool, FinishReason, FixedClock, MemoryStore, Model,
    ModelDelta, ModelRequest, Policy, PolicyRequest, Store, Tool, ToolCtx, Verdict,
};
use nucleos::contract::{
    Content, ErrorCode, Event, EventKind, Message, Part, PartKind, Principal, Role, SessionId,
    SessionState, Snapshot, StopReason, TextPart, Usage,
};
use tokio_stream::StreamExt;

fn event(session: &SessionId, seq: u64) -> Event {
    Event {
        seq,
        session: session.clone(),
        at_unix_millis: 1_000 + seq,
        schema_version: Event::SCHEMA_VERSION,
        kind: EventKind::TextDelta {
            part_index: 0,
            text: format!("chunk {seq}"),
        },
        meta: Default::default(),
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn call() -> nucleos::ToolCall {
    nucleos::ToolCall {
        id: "call-1".to_owned(),
        name: "read_file".to_owned(),
        arguments_json: "{\"path\":\"README.md\"}".to_owned(),
        meta: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_streams_the_deltas_it_was_scripted_with() -> Result<()> {
    let model = FakeModel::text(["Hel", "lo"]);
    let mut stream = model.invoke(ModelRequest::default()).await?;

    let mut text = String::new();
    let mut finished = None;
    while let Some(delta) = stream.next().await {
        match delta? {
            ModelDelta::Text { text: chunk, .. } => text.push_str(&chunk),
            ModelDelta::Finished { reason } => finished = Some(reason),
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    assert_eq!(text, "Hello");
    assert_eq!(finished, Some(FinishReason::Stop));
    Ok(())
}

#[tokio::test]
async fn an_exhausted_script_stops_instead_of_streaming_forever() -> Result<()> {
    let model = FakeModel::text(["once"]);

    let mut first = model.invoke(ModelRequest::default()).await?;
    while first.next().await.transpose()?.is_some() {}

    // The script is spent: the second call must terminate, not hang. A fake that streams
    // forever turns a bug in the engine's stopping logic into a timeout.
    let mut second = model.invoke(ModelRequest::default()).await?;
    let mut stopped = false;
    while let Some(delta) = second.next().await {
        if let ModelDelta::Finished { reason } = delta? {
            assert_eq!(reason, FinishReason::Stop);
            stopped = true;
        }
    }

    assert!(stopped, "an exhausted script must still finish");
    assert_eq!(model.calls(), 2);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_tool_answers_with_content_not_an_error() {
    let tool = FakeTool::new("read_file", "unused").failing("no such file: README.md");
    let session = SessionId::new("s1");
    let cx = ToolCtx {
        session: &session,
        call_id: "call-1",
        deadline_unix_millis: 9_000,
    };

    let output = tool.invoke("{}", &cx).await;

    assert!(
        output.is_error,
        "the flag is how the model learns it failed"
    );
    assert_eq!(
        output.content.text(),
        "no such file: README.md",
        "the reason must be content the model can read"
    );
}

#[tokio::test]
async fn a_tool_reports_the_isolation_it_was_built_with() {
    let tool = FakeTool::new("read_file", "ok");
    assert_eq!(tool.isolation(), nucleos::Isolation::None);
    assert_eq!(tool.spec().name, "read_file");
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_store_rejects_a_gap_rather_than_renumbering() -> Result<()> {
    let store = MemoryStore::new();
    let session = SessionId::new("s1");

    store.append(&session, &[event(&session, 0)]).await?;

    // 1 is missing. A store that silently renumbered would make every later resume cursor
    // point at the wrong event.
    let error = store
        .append(&session, &[event(&session, 2)])
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("does not continue"));
    Ok(())
}

#[tokio::test]
async fn events_since_is_inclusive() -> Result<()> {
    let store = MemoryStore::new();
    let session = SessionId::new("s1");
    store
        .append(
            &session,
            &[event(&session, 0), event(&session, 1), event(&session, 2)],
        )
        .await?;

    // Inclusive, because a client asking `from_seq = 1` wants event 1: anything else makes
    // the ABI's resume cursor mean "after" while it says "from".
    let tail = store.events_since(&session, 1).await?;
    let seqs: Vec<u64> = tail.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn a_store_refuses_an_event_belonging_to_another_session() -> Result<()> {
    let store = MemoryStore::new();
    let mine = SessionId::new("s1");
    let theirs = SessionId::new("s2");

    let error = store.append(&mine, &[event(&theirs, 0)]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn a_snapshot_cannot_claim_more_than_the_log_holds() -> Result<()> {
    let store = MemoryStore::new();
    let session = SessionId::new("s1");
    store.append(&session, &[event(&session, 0)]).await?;

    let snapshot = Snapshot {
        session: session.clone(),
        seq: 3,
        generation: nucleos::Generation(1),
        state: SessionState::Running,
        messages: Vec::new(),
        usage: Usage::default(),
        steps: 1,
    };

    let error = store.write_snapshot(snapshot).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn a_valid_snapshot_round_trips() -> Result<()> {
    let store = MemoryStore::new();
    let session = SessionId::new("s1");
    store.append(&session, &[event(&session, 0)]).await?;

    let message = Message {
        role: Role::Assistant,
        content: Content::new([Part::new(PartKind::Text(TextPart {
            text: "hi".to_owned(),
        }))]),
    };
    store.write_snapshot(Snapshot {
        session: session.clone(),
        seq: 0,
        generation: nucleos::Generation(7),
        state: SessionState::Done,
        messages: vec![message.clone()],
        usage: Usage {
            input_tokens: 10,
            output_tokens: 2,
            micros_usd: Some(42),
        },
        steps: 1,
    })?;

    let loaded = store.snapshot(&session).await?.unwrap();
    assert_eq!(loaded.seq, 0, "seq is the last event included");
    assert_eq!(loaded.generation, nucleos::Generation(7));
    assert_eq!(loaded.messages, vec![message]);
    assert_eq!(loaded.usage.micros_usd, Some(42));
    Ok(())
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_answers_all_three_ways() {
    let session = SessionId::new("s1");
    let who = principal();
    let call = call();
    let request = PolicyRequest {
        principal: &who,
        session: &session,
        call: &call,
    };

    assert_eq!(AllowAll.check(request).await, Verdict::Allow);

    let request = PolicyRequest {
        principal: &who,
        session: &session,
        call: &call,
    };
    match DenyAll.check(request).await {
        Verdict::Deny { code, reason } => {
            assert_eq!(code, ErrorCode::Denied);
            assert!(reason.contains("read_file"));
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

#[test]
fn the_clock_moves_only_when_told() {
    let clock = FixedClock::at(1_700_000_000_000);
    assert_eq!(clock.now_unix_millis(), 1_700_000_000_000);
    assert_eq!(
        clock.now_unix_millis(),
        1_700_000_000_000,
        "reading is pure"
    );
    clock.advance(250);
    assert_eq!(clock.now_unix_millis(), 1_700_000_000_250);
}

#[test]
fn the_system_clock_reports_something_plausible() {
    // Not a tautology: a kernel that aborts when a machine's clock is wrong turns a broken
    // test environment into an outage.
    assert!(nucleos::adapter::SystemClock.now_unix_millis() > 0);
}

// ---------------------------------------------------------------------------
// Content model
// ---------------------------------------------------------------------------

#[test]
fn content_text_joins_every_text_part_and_skips_the_rest() {
    let content = Content::new([
        Part::new(PartKind::Text(TextPart {
            text: "a".to_owned(),
        })),
        Part::new(PartKind::Reasoning(nucleos::contract::ReasoningPart {
            text: "thinking".to_owned(),
            signature: "sig".to_owned(),
            id: String::new(),
            source: String::new(),
        })),
        Part::new(PartKind::Text(TextPart {
            text: "b".to_owned(),
        })),
    ]);

    // One definition, because "first text part" for one caller and "all text parts joined"
    // for another is how two readers of the same value start disagreeing.
    assert_eq!(content.text(), "ab");
}

#[test]
fn an_event_reports_a_routable_type() {
    let session = SessionId::new("s1");
    let event = event(&session, 0);
    assert_eq!(event.kind.type_name(), "nucleos.v1.text_delta");

    let terminal = EventKind::StateChanged {
        state: SessionState::Done,
        stop: Some(StopReason::Completed),
    };
    assert_eq!(terminal.type_name(), "nucleos.v1.state_changed");
}
