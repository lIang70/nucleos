//! The contracts, exercised through their fakes.
//!
//! These are not tests of the fakes; they are tests of the *rules the traits and types state*,
//! using the fakes as the cheapest way to state them. When a real adapter disagrees with one
//! of these, the trait was wrong, not the test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::Result;
use nucleos::adapter::{
    AllowAll, Clock, DenyAll, FinishReason, Gated, Model, ModelDelta, ModelRegistry, ModelRequest,
    Policy, PolicyRequest, Store, Tool, ToolChoice, ToolCtx, Verdict,
};
use nucleos::contract::{
    Attempt, AttemptOutcome, AttemptPhase, AttemptTrigger, Content, ErrorCode, Event, EventKind,
    Ledger, Message, Part, PartKind, Principal, Role, Routing, SessionId, SessionState, Snapshot,
    StopReason, TextPart, Usage,
};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{EchoModel, FakeModel, FakeTool, FixedClock, tool_call_deltas};

/// Compile then send, the way the kernel does: there is no way to invoke an uncompiled request.
async fn stream(
    model: &impl Model,
    request: ModelRequest,
) -> Result<nucleos::adapter::ModelStream> {
    let prepared = model.prepare(request)?;
    model.invoke(prepared).await
}
use std::sync::Arc;
use tokio_stream::StreamExt;

/// The store's key for a test tenant. The tests all use one tenant, so the helper keeps them about
/// what they are testing rather than about spelling out a key at every call.
fn key(id: &SessionId) -> nucleos::adapter::SessionKey {
    nucleos::adapter::SessionKey::new("acme", id.clone())
}

fn event(session: &SessionId, seq: u64) -> Event {
    Event {
        seq,
        session: session.clone(),
        at_unix_millis: 1_000 + seq,
        schema_version: Event::SCHEMA_VERSION,
        kind: EventKind::PartAppend {
            part_index: 0,
            part: nucleos::contract::Part::text(format!("chunk {seq}")),
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

fn attempt(index: u32, ok: bool) -> Attempt {
    Attempt {
        index,
        model: "primary".to_owned(),
        phase: AttemptPhase::Execute,
        trigger: if index == 0 {
            AttemptTrigger::Selection
        } else {
            AttemptTrigger::Retry
        },
        outcome: if ok {
            AttemptOutcome::Ok
        } else {
            AttemptOutcome::Failed {
                code: ErrorCode::Unavailable,
                reason: "503".to_owned(),
            }
        },
        usage: Usage {
            input_tokens: 10,
            output_tokens: 0,
            micros_usd: Some(5),
        },
        duration_millis: 3,
        degraded: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_streams_the_deltas_it_was_scripted_with() -> Result<()> {
    let model = FakeModel::text(["He", "llo"]);
    let mut stream = stream(&model, ModelRequest::default()).await?;

    let mut text = String::new();
    let mut finished = None;
    while let Some(delta) = stream.next().await {
        match delta? {
            ModelDelta::PartAppend { part, .. } => {
                if let nucleos::contract::PartKind::Text(chunk) = &part.kind {
                    text.push_str(&chunk.text);
                }
            }
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

    let mut first = stream(&model, ModelRequest::default()).await?;
    while first.next().await.transpose()?.is_some() {}

    // The script is spent: the second call must terminate, not hang. A fake that streams
    // forever turns a bug in a driver's stopping logic into a timeout.
    let mut second = stream(&model, ModelRequest::default()).await?;
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

#[tokio::test]
async fn tool_arguments_stream_as_fragments_addressed_by_index() -> Result<()> {
    // The shape a provider actually sends: a start, then argument text, then a finish. The
    // kernel assembles the fragments by index and validates the JSON before anything runs.
    let model = FakeModel::with_script(vec![tool_call_deltas(
        "call-1",
        "read_file",
        "{\"path\":\"a\"}",
    )]);
    let mut stream = stream(&model, ModelRequest::default()).await?;

    let mut started = None;
    let mut assembled = String::new();
    let mut finished = None;
    while let Some(delta) = stream.next().await {
        match delta? {
            ModelDelta::ToolCallStarted {
                index, id, name, ..
            } => started = Some((index, id, name)),
            ModelDelta::ToolCallArgs { index, fragment } => {
                assert_eq!(index, 0);
                assembled.push_str(&fragment);
            }
            ModelDelta::Finished { reason } => finished = Some(reason),
            other => panic!("unexpected delta: {other:?}"),
        }
    }

    assert_eq!(
        started,
        Some((0, "call-1".to_owned(), "read_file".to_owned()))
    );
    assert_eq!(assembled, "{\"path\":\"a\"}");
    assert_eq!(finished, Some(FinishReason::ToolCalls));
    Ok(())
}

#[test]
fn token_counting_is_the_adapters_decision_not_the_kernels() -> Result<()> {
    let request = ModelRequest {
        system: Some("be brief".to_owned()),
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::new(PartKind::Text(TextPart {
                text: "hello there".to_owned(),
            }))]),
        }]
        .into(),
        ..ModelRequest::default()
    };

    // `text_len` counts characters across system and every message, once, so that two
    // adapters cannot disagree about what "the text of a request" means.
    assert_eq!(request.text_len(), 8 + 11);
    assert_eq!(
        EchoModel::new("hi").count_tokens(&request)?,
        19u64.div_ceil(4)
    );

    // An adapter with an exact tokenizer says so instead of estimating.
    let exact = FakeModel::text(["hi"]).with_token_count(4_096);
    assert_eq!(exact.count_tokens(&request)?, 4_096);
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing and the ledger
// ---------------------------------------------------------------------------

#[test]
fn a_route_resolves_in_order_or_fails_loudly() -> Result<()> {
    let mut registry = ModelRegistry::new();
    registry.insert("primary", Arc::new(EchoModel::new("a")));
    registry.insert("secondary", Arc::new(EchoModel::new("b")));

    let resolved = registry.resolve(&Routing {
        chain: vec!["secondary".to_owned(), "primary".to_owned()],
        retries: 1,
    })?;
    let names: Vec<&str> = resolved.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["secondary", "primary"]);

    // A typo in a route is an error, not a skip: skipping it would be discovered as "the model
    // behaved differently", by which point the session has already run.
    let error = registry
        .resolve(&Routing {
            chain: vec!["primray".to_owned()],
            retries: 0,
        })
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::UnknownModel);

    let error = registry
        .resolve(&Routing {
            chain: Vec::new(),
            retries: 0,
        })
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[test]
fn the_ledger_counts_every_attempt_that_spent_money() {
    let mut ledger = Ledger::default();
    ledger.record(attempt(0, false));
    ledger.record(attempt(1, true));

    assert_eq!(ledger.len(), 2);
    assert_eq!(
        ledger.total.input_tokens, 20,
        "a failed attempt that produced tokens still paid for them"
    );
    assert_eq!(ledger.total.micros_usd, Some(10));
}

#[test]
fn an_unreported_cost_stays_unreported_through_the_ledger() {
    let mut ledger = Ledger::default();
    ledger.record(attempt(0, true));

    let mut unreported = attempt(1, true);
    unreported.usage.micros_usd = None;
    ledger.record(unreported);

    assert_eq!(
        ledger.total.micros_usd, None,
        "adding an unknown as zero under-reports the bill"
    );
    assert_eq!(ledger.total.input_tokens, 20, "tokens still add up");
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
        progress: &nucleos::adapter::NoProgress,
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

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_store_rejects_a_gap_rather_than_renumbering() -> Result<()> {
    let store = SqliteStore::in_memory().expect("an in-memory database");
    let session = SessionId::new("s1");

    store.append(&key(&session), &[event(&session, 0)]).await?;

    // 1 is missing. A store that silently renumbered would make every later resume cursor
    // point at the wrong event.
    let error = store
        .append(&key(&session), &[event(&session, 2)])
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("does not continue"));
    Ok(())
}

#[tokio::test]
async fn events_since_is_inclusive() -> Result<()> {
    let store = SqliteStore::in_memory().expect("an in-memory database");
    let session = SessionId::new("s1");
    store
        .append(
            &key(&session),
            &[event(&session, 0), event(&session, 1), event(&session, 2)],
        )
        .await?;

    // Inclusive, because a client asking `from_seq = 1` wants event 1: anything else makes the
    // ABI's resume cursor mean "after" while it says "from".
    let tail = store.events_since(&key(&session), 1).await?;
    let seqs: Vec<u64> = tail.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn a_store_refuses_an_event_belonging_to_another_session() -> Result<()> {
    let store = SqliteStore::in_memory().expect("an in-memory database");
    let mine = SessionId::new("s1");
    let theirs = SessionId::new("s2");

    let error = store
        .append(&key(&mine), &[event(&theirs, 0)])
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn a_snapshot_cannot_claim_more_than_the_log_holds() -> Result<()> {
    let store = SqliteStore::in_memory().expect("an in-memory database");
    let session = SessionId::new("s1");
    store.append(&key(&session), &[event(&session, 0)]).await?;

    let snapshot = Snapshot {
        principal: Principal {
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        },
        session: session.clone(),
        seq: 3,
        version: 1,
        generation: nucleos::Generation(1),
        state: SessionState::Open,
        messages: Vec::new(),
        usage: Usage::default(),
        ledger: Default::default(),
        inference_calls: 1,
        credit_tokens: Some(1_000),
        budget: Default::default(),
        spent_wall_millis: 0,
        pending: Vec::new(),
        routing: Default::default(),
        system: None,
        tools: Vec::new(),
        params: Default::default(),
        data: Default::default(),
        workspace: None,
        sandbox: Default::default(),
        hints: Default::default(),
        tool_policy: Default::default(),
        discovered: Default::default(),
        round: 0,
        stop: None,
        kind: Default::default(),
        process: None,
    };

    let error = store.write_snapshot(snapshot).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn a_valid_snapshot_round_trips() -> Result<()> {
    let store = SqliteStore::in_memory().expect("an in-memory database");
    let session = SessionId::new("s1");
    store.append(&key(&session), &[event(&session, 0)]).await?;

    let message = Message {
        role: Role::Assistant,
        content: Content::new([Part::new(PartKind::Text(TextPart {
            text: "hi".to_owned(),
        }))]),
    };
    store
        .write_snapshot(Snapshot {
            principal: Principal {
                tenant: "acme".to_owned(),
                subject: "alice".to_owned(),
            },
            session: session.clone(),
            seq: 0,
            version: 1,
            generation: nucleos::Generation(7),
            state: SessionState::Closed,
            messages: vec![message.clone()],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 2,
                micros_usd: Some(42),
            },
            ledger: Default::default(),
            inference_calls: 1,
            credit_tokens: Some(1_000),
            budget: Default::default(),
            spent_wall_millis: 0,
            pending: Vec::new(),
            routing: Default::default(),
            system: None,
            tools: Vec::new(),
            params: Default::default(),
            data: Default::default(),
            workspace: None,
            sandbox: Default::default(),
            hints: Default::default(),
            tool_policy: Default::default(),
            discovered: Default::default(),
            round: 0,
            stop: None,
            kind: Default::default(),
            process: None,
        })
        .await?;

    let Some(loaded) = store.snapshot(&key(&session)).await? else {
        return Err(nucleos::Error::internal(
            "the snapshot just written is missing",
        ));
    };
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
async fn policy_gates_both_tools_and_model_calls() {
    let session = SessionId::new("s1");
    let who = principal();
    let call = call();

    let tool_gate = PolicyRequest {
        principal: &who,
        session: Some(&session),
        subject: Gated::Tool { call: &call },
    };
    assert_eq!(AllowAll.check(tool_gate).await, Verdict::Allow);

    // The same trait answers the money question. A runtime that gates tools but not model
    // calls has a policy with a hole where the spend is.
    let model_gate = PolicyRequest {
        principal: &who,
        session: None,
        subject: Gated::Model {
            model: "primary",
            purpose: nucleos::contract::Purpose::Answer,
        },
    };
    match DenyAll.check(model_gate).await {
        Verdict::Deny { code, reason } => {
            assert_eq!(code, ErrorCode::Denied);
            assert!(reason.contains("primary"));
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn the_default_tool_choice_is_the_models_own() {
    // `Required` exists for the case where the model explains the problem instead of calling a
    // tool; it must be opt-in, because forcing a tool on a task that needs none is worse.
    assert_eq!(
        nucleos::adapter::GenerateParams::default().tool_choice,
        ToolChoice::Auto
    );
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

    // One definition, because "first text part" for one caller and "all text parts joined" for
    // another is how two readers of the same value start disagreeing.
    assert_eq!(content.text(), "ab");
}

#[test]
fn an_event_reports_a_routable_type() {
    let session = SessionId::new("s1");
    let event = event(&session, 0);
    assert_eq!(event.kind.type_name(), "nucleos.v1.part_append");

    let terminal = EventKind::StateChanged {
        state: SessionState::Closed,
        stop: Some(StopReason::Completed),
    };
    assert_eq!(terminal.type_name(), "nucleos.v1.state_changed");
}

/// A capability declaration is checked before it becomes a model a route can name.
#[test]
fn a_capability_must_declare_outputs_it_can_actually_serve() {
    let capability = |outputs| nucleos::contract::Capability {
        inputs: Vec::new(),
        outputs,
        tool_calling: true,
        tool_choice_required: true,
        structured_output: true,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: None,
    };
    use nucleos::contract::PartKindName;

    // Text and audio are modalities, and declaring both is fine.
    assert!(
        capability(vec![PartKindName::Text, PartKindName::Audio])
            .validate()
            .is_ok()
    );

    // A tool call is not an output modality: a call asks for text, an image, audio or video.
    let error = capability(vec![PartKindName::ToolCall])
        .validate()
        .unwrap_err();
    assert!(error.contains("not an output modality"), "{error}");

    // Nor is the same modality twice, which would make the declaration ambiguous to a reader.
    let error = capability(vec![PartKindName::Text, PartKindName::Text])
        .validate()
        .unwrap_err();
    assert!(error.contains("twice"), "{error}");
}

/// A degradation with no explanation is a second silent failure, not a report.
#[test]
fn a_degradation_must_explain_itself() {
    let entry = nucleos::contract::Degraded {
        field: nucleos::contract::Field::System,
        reason: "  ".to_owned(),
        code: None,
    };
    assert!(entry.validate().is_err());
}

/// The adapter-facing conformance check runs without a kernel or a store.
#[tokio::test]
async fn the_conformance_check_accepts_a_well_behaved_model() -> Result<()> {
    let model = FakeModel::text(["ok"]);
    nucleos::testing::conformance(&model, ModelRequest::default()).await
}

/// The terminal event is terminal: a delta after `Finished` is an adapter contract violation.
#[tokio::test]
async fn the_conformance_check_rejects_content_after_the_finish() {
    let model = FakeModel::with_script(vec![vec![
        ModelDelta::Finished {
            reason: FinishReason::Stop,
        },
        ModelDelta::PartAppend {
            part_index: 0,
            part: Part::text("too late"),
        },
    ]]);
    let error = nucleos::testing::conformance(&model, ModelRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::AdapterContractViolation);
}

/// The delta guard is the one place the stream's *shape* is decided.
mod stream_shape {
    use super::*;
    use nucleos::adapter::DeltaGuard;

    fn text_at(index: u32) -> ModelDelta {
        ModelDelta::PartAppend {
            part_index: index,
            part: Part::text("x"),
        }
    }

    fn call_at(call: u32, part: u32) -> ModelDelta {
        ModelDelta::ToolCallStarted {
            index: call,
            part_index: part,
            id: format!("call-{call}"),
            name: "read_file".to_owned(),
        }
    }

    #[test]
    fn a_well_formed_stream_is_accepted() {
        let mut guard = DeltaGuard::new();
        for delta in [
            text_at(0),
            call_at(0, 1),
            ModelDelta::ToolCallArgs {
                index: 0,
                fragment: "{}".to_owned(),
            },
            text_at(2),
            ModelDelta::Finished {
                reason: FinishReason::ToolCalls,
            },
        ] {
            guard.accept(&delta).unwrap();
        }
        assert!(guard.finished());
        assert_eq!(guard.finish(), Some(&FinishReason::ToolCalls));
    }

    /// The terminal event is terminal.
    #[test]
    fn content_after_the_finish_is_refused() {
        let mut guard = DeltaGuard::new();
        guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let error = guard.accept(&text_at(0)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::AdapterContractViolation);
        assert!(error.reason().contains("after its terminal"), "{error}");
    }

    /// Fragments for a call nobody announced would assemble into a call with no name — and, before
    /// the guard, into a call that vanished.
    #[test]
    fn arguments_for_an_unannounced_call_are_refused() {
        let mut guard = DeltaGuard::new();
        let error = guard
            .accept(&ModelDelta::ToolCallArgs {
                index: 7,
                fragment: "{}".to_owned(),
            })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::AdapterContractViolation);
        assert!(error.reason().contains("never announced"), "{error}");
    }

    /// Two calls at one part index means one of them disappears from the message.
    #[test]
    fn two_calls_cannot_claim_one_part() {
        let mut guard = DeltaGuard::new();
        guard.accept(&call_at(0, 1)).unwrap();
        let error = guard.accept(&call_at(1, 1)).unwrap_err();
        assert!(error.reason().contains("claim part 1"), "{error}");
    }

    /// Neither can a call and a content fragment, which would overwrite each other.
    #[test]
    fn a_call_cannot_share_a_part_with_content() {
        let mut guard = DeltaGuard::new();
        guard.accept(&text_at(1)).unwrap();
        let error = guard.accept(&call_at(0, 1)).unwrap_err();
        assert!(error.reason().contains("content fragment"), "{error}");
    }

    /// The same call announced twice cannot be attributed to one stream of arguments.
    #[test]
    fn a_call_announced_twice_is_refused() {
        let mut guard = DeltaGuard::new();
        guard.accept(&call_at(0, 1)).unwrap();
        let error = guard.accept(&call_at(0, 2)).unwrap_err();
        assert!(error.reason().contains("announced twice"), "{error}");
    }
}
