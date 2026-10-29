//! The engine, end to end, on fakes.
//!
//! Every test here answers one question about a rule in `docs/architecture.md`: does the loop
//! stop where it says it stops, does the ledger count what it says it counts, does a parked
//! session come back.

#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FakeModel, FakeTool, FixedClock, Gated, MemoryStore, Model, ModelDelta, Policy,
    PolicyRequest, Store, Verdict, fake::tool_call_deltas,
};
use nucleos::contract::{
    AttemptOutcome, Budget, ErrorCode, EventKind, PermissionKind, PermissionOption, Principal,
    Routing, SessionId, SessionState, StopReason,
};
use nucleos::engine::{HitlVerdict, Kernel, SessionSpec};
use nucleos::{Error, Result};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
        routing: Some(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        }),
        credit_tokens: 1_000_000,
        ..SessionSpec::default()
    }
}

fn kernel(model: FakeModel) -> (Kernel, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(model))
        .tool(Arc::new(FakeTool::new("read_file", "contents")))
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(1_700_000_000_000)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (kernel, store)
}

fn kinds(store: &MemoryStore, session: &SessionId) -> Vec<&'static str> {
    store
        .recorded(session)
        .iter()
        .map(|event| event.kind.type_name())
        .collect()
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_turn_session_answers_and_checkpoints() -> Result<()> {
    let (kernel, store) = kernel(FakeModel::text(["hello"]));
    let id = SessionId::new("s1");

    let outcome = kernel.start(&principal(), id.clone(), spec()).await?;

    assert_eq!(outcome.state, SessionState::Done);
    assert_eq!(outcome.stop, Some(StopReason::Completed));
    assert!(outcome.is_terminal());

    let seen = kinds(&store, &id);
    assert_eq!(
        seen,
        vec![
            "nucleos.v1.state_changed",
            "nucleos.v1.text_delta",
            "nucleos.v1.message_completed",
            "nucleos.v1.state_changed",
        ]
    );

    let snapshot = store.snapshot(&id).await?.unwrap();
    assert_eq!(snapshot.state, SessionState::Done);
    assert_eq!(snapshot.steps, 1);
    assert_eq!(
        snapshot.seq,
        outcome.next_seq - 1,
        "a checkpoint includes exactly the events behind it"
    );
    Ok(())
}

#[tokio::test]
async fn a_tool_call_runs_and_the_model_answers_afterwards() -> Result<()> {
    let model = FakeModel::with_script(vec![
        tool_call_deltas("call-1", "read_file", "{\"path\":\"README.md\"}"),
        vec![
            ModelDelta::Text {
                part_index: 0,
                text: "the file says hi".to_owned(),
            },
            ModelDelta::Finished {
                reason: nucleos::adapter::FinishReason::Stop,
            },
        ],
    ]);
    let (kernel, store) = kernel(model);
    let id = SessionId::new("s1");

    let outcome = kernel.start(&principal(), id.clone(), spec()).await?;

    assert_eq!(outcome.state, SessionState::Done);
    let seen = kinds(&store, &id);
    assert!(seen.contains(&"nucleos.v1.tool_call_started"));
    assert!(seen.contains(&"nucleos.v1.tool_call_args"));
    assert!(seen.contains(&"nucleos.v1.tool_finished"));
    assert_eq!(
        seen.iter()
            .filter(|kind| **kind == "nucleos.v1.message_completed")
            .count(),
        2,
        "one message for the tool call, one for the answer"
    );

    let snapshot = store.snapshot(&id).await?.unwrap();
    assert_eq!(snapshot.steps, 2);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.role == nucleos::Role::Tool),
        "the tool result is in history"
    );
    Ok(())
}

#[tokio::test]
async fn malformed_arguments_never_reach_the_tool() -> Result<()> {
    let model = FakeModel::with_script(vec![
        tool_call_deltas("call-1", "read_file", "{\"path\":"),
        vec![ModelDelta::Finished {
            reason: nucleos::adapter::FinishReason::Stop,
        }],
    ]);
    let (kernel, store) = kernel(model);
    let id = SessionId::new("s1");

    kernel.start(&principal(), id.clone(), spec()).await?;

    let result = store
        .recorded(&id)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ToolFinished { result, .. } => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(result.content.text().contains("not valid JSON"));
    Ok(())
}

#[tokio::test]
async fn a_tool_that_reports_a_cost_is_charged_for_it() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model(
            "primary",
            Arc::new(FakeModel::with_script(vec![
                tool_call_deltas("call-1", "read_file", "{}"),
                vec![ModelDelta::Finished {
                    reason: nucleos::adapter::FinishReason::Stop,
                }],
            ])),
        )
        .tool(Arc::new(
            FakeTool::new("read_file", "contents").reporting_usage(nucleos::Usage {
                input_tokens: 0,
                output_tokens: 0,
                micros_usd: Some(250),
            }),
        ))
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();

    let id = SessionId::new("s1");
    kernel.start(&principal(), id.clone(), spec()).await?;

    // A tool that calls a paid API can say what it cost, and the kernel attributes it. The
    // alternative — a gateway endpoint so the kernel could make the call itself — was a second
    // product, not a fix.
    let charged = store
        .recorded(&id)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ToolFinished { usage, .. } => usage,
            _ => None,
        })
        .unwrap();
    assert_eq!(charged.micros_usd, Some(250));
    Ok(())
}

#[tokio::test]
async fn a_tool_that_reports_nothing_is_not_assumed_free() -> Result<()> {
    let (kernel, store) = kernel(FakeModel::with_script(vec![
        tool_call_deltas("call-1", "read_file", "{}"),
        vec![ModelDelta::Finished {
            reason: nucleos::adapter::FinishReason::Stop,
        }],
    ]));
    let id = SessionId::new("s1");
    kernel.start(&principal(), id.clone(), spec()).await?;

    let usage = store
        .recorded(&id)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ToolFinished { usage, .. } => Some(usage),
            _ => None,
        })
        .flatten();
    assert!(usage.is_none(), "silence is not a zero, it is an absence");
    Ok(())
}

// ---------------------------------------------------------------------------
// Stop conditions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_step_cap_stops_a_session_that_would_loop() -> Result<()> {
    // A model that always asks for a tool never finishes on its own: only the step cap ends it.
    let forever = tool_call_deltas("call-1", "read_file", "{}");
    let model = FakeModel::with_script(vec![forever.clone(), forever.clone(), forever]);
    let (kernel, store) = kernel(model);
    let id = SessionId::new("s1");

    let outcome = kernel
        .start(
            &principal(),
            id.clone(),
            SessionSpec {
                budget: Budget {
                    max_steps: Some(1),
                    ..Budget::default()
                },
                ..spec()
            },
        )
        .await?;

    assert_eq!(outcome.state, SessionState::Failed);
    assert_eq!(outcome.stop, Some(StopReason::Steps));
    assert!(kinds(&store, &id).contains(&"nucleos.v1.budget_exhausted"));
    Ok(())
}

#[tokio::test]
async fn no_credit_means_no_model_call() -> Result<()> {
    let (kernel, store) = kernel(FakeModel::text(["never asked"]));
    let id = SessionId::new("s1");

    let outcome = kernel
        .start(
            &principal(),
            id.clone(),
            SessionSpec {
                credit_tokens: 0,
                ..spec()
            },
        )
        .await?;

    assert_eq!(outcome.stop, Some(StopReason::Tokens));
    let seen = kinds(&store, &id);
    assert!(!seen.contains(&"nucleos.v1.text_delta"), "no call ran");
    Ok(())
}

#[tokio::test]
async fn a_truncated_answer_stops_with_the_token_reason() -> Result<()> {
    let model = FakeModel::with_script(vec![vec![
        ModelDelta::Text {
            part_index: 0,
            text: "half a sen".to_owned(),
        },
        ModelDelta::Finished {
            reason: nucleos::adapter::FinishReason::Length,
        },
    ]]);
    let (kernel, _) = kernel(model);

    let outcome = kernel
        .start(&principal(), SessionId::new("s1"), spec())
        .await?;
    assert_eq!(outcome.stop, Some(StopReason::Tokens));
    assert_eq!(outcome.state, SessionState::Failed);
    Ok(())
}

// ---------------------------------------------------------------------------
// Routing and the ledger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_primary_falls_back_and_both_attempts_are_recorded() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(Broken))
        .model(
            "secondary",
            Arc::new(FakeModel::text(["answered by the fallback"])),
        )
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();

    let outcome = kernel
        .start(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                routing: Some(Routing {
                    chain: vec!["primary".to_owned(), "secondary".to_owned()],
                    retries: 0,
                }),
                ..spec()
            },
        )
        .await?;

    assert_eq!(outcome.state, SessionState::Done);
    assert_eq!(outcome.ledger.len(), 2);
    assert!(matches!(
        outcome.ledger.attempts[0].outcome,
        AttemptOutcome::Failed { .. }
    ));
    assert!(matches!(
        outcome.ledger.attempts[1].outcome,
        AttemptOutcome::Ok
    ));
    Ok(())
}

#[tokio::test]
async fn an_attempt_that_streamed_is_not_retried() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(StreamsThenFails))
        .model("secondary", Arc::new(FakeModel::text(["second answer"])))
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();

    let outcome = kernel
        .start(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                routing: Some(Routing {
                    chain: vec!["primary".to_owned(), "secondary".to_owned()],
                    retries: 2,
                }),
                ..spec()
            },
        )
        .await?;

    // Retrying would put two partial answers in one turn's history, and no replay could tell
    // which one the model was continuing.
    assert_eq!(outcome.state, SessionState::Failed);
    assert_eq!(
        outcome.ledger.len(),
        1,
        "one attempt, no retry, no fallback"
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_route_target_is_refused() {
    let (kernel, _) = kernel(FakeModel::text(["hi"]));
    let error = kernel
        .start(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                routing: Some(Routing {
                    chain: vec!["nobody".to_owned()],
                    retries: 0,
                }),
                ..spec()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::NotFound);
}

// ---------------------------------------------------------------------------
// Policy and the human in the loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_denied_tool_answers_with_content_and_the_turn_continues() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model(
            "primary",
            Arc::new(FakeModel::with_script(vec![
                tool_call_deltas("call-1", "read_file", "{}"),
                vec![ModelDelta::Text {
                    part_index: 0,
                    text: "understood".to_owned(),
                }],
            ])),
        )
        .tool(Arc::new(FakeTool::new("read_file", "contents")))
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(DenyToolsOnly))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();

    let outcome = kernel
        .start(&principal(), SessionId::new("s1"), spec())
        .await?;

    // A refusal is content the model reads, not a failed turn: the session keeps going.
    assert_eq!(outcome.state, SessionState::Done);
    let refusal = store
        .recorded(&SessionId::new("s1"))
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::ToolFinished { result, .. } => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(refusal.is_error);
    Ok(())
}

#[tokio::test]
async fn a_gated_call_parks_the_session_and_a_verdict_resumes_it() -> Result<()> {
    let store = Arc::new(MemoryStore::new());
    let model = FakeModel::with_script(vec![
        tool_call_deltas("call-1", "read_file", "{}"),
        vec![ModelDelta::Text {
            part_index: 0,
            text: "the file says hi".to_owned(),
        }],
    ]);
    let kernel = Kernel::builder()
        .model("primary", Arc::new(model))
        .tool(Arc::new(FakeTool::new("read_file", "contents")))
        .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
        .policy(Arc::new(AskFirst))
        .clock(Arc::new(FixedClock::at(0)))
        .build()
        .unwrap();

    let id = SessionId::new("s1");
    let parked = kernel.start(&principal(), id.clone(), spec()).await?;
    assert_eq!(parked.state, SessionState::AwaitingHuman);
    assert_eq!(parked.stop, None, "parked is not stopped");

    let snapshot = store.snapshot(&id).await?.unwrap();
    assert!(snapshot.pending.is_some(), "the call survives the process");

    let resumed = kernel
        .answer(
            &principal(),
            id.clone(),
            spec(),
            HitlVerdict {
                tool_call_id: "call-1".to_owned(),
                option_id: "allow".to_owned(),
            },
        )
        .await?;

    assert_eq!(resumed.state, SessionState::Done);
    Ok(())
}

#[tokio::test]
async fn a_verdict_for_an_option_that_was_never_offered_is_refused() -> Result<()> {
    let (kernel, _) = {
        let store = Arc::new(MemoryStore::new());
        let kernel = Kernel::builder()
            .model(
                "primary",
                Arc::new(FakeModel::with_script(vec![tool_call_deltas(
                    "call-1",
                    "read_file",
                    "{}",
                )])),
            )
            .tool(Arc::new(FakeTool::new("read_file", "contents")))
            .store(Arc::clone(&store) as Arc<dyn nucleos::adapter::Store>)
            .policy(Arc::new(AskFirst))
            .clock(Arc::new(FixedClock::at(0)))
            .build()
            .unwrap();
        (kernel, store)
    };

    let id = SessionId::new("s1");
    kernel.start(&principal(), id.clone(), spec()).await?;

    let error = kernel
        .answer(
            &principal(),
            id,
            spec(),
            HitlVerdict {
                tool_call_id: "call-1".to_owned(),
                option_id: "not-offered".to_owned(),
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

// ---------------------------------------------------------------------------
// Adapters that exist only to fail in a specific way
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Broken;

#[async_trait]
impl Model for Broken {
    fn capability(&self) -> nucleos::Capability {
        FakeModel::text(["x"]).capability()
    }

    fn count_tokens(&self, _request: &nucleos::adapter::ModelRequest) -> Result<u64> {
        Ok(1)
    }

    async fn invoke(
        &self,
        _request: nucleos::adapter::ModelRequest,
    ) -> Result<nucleos::adapter::ModelStream> {
        Err(Error::unavailable("provider is down"))
    }
}

#[derive(Debug)]
struct StreamsThenFails;

#[async_trait]
impl Model for StreamsThenFails {
    fn capability(&self) -> nucleos::Capability {
        FakeModel::text(["x"]).capability()
    }

    fn count_tokens(&self, _request: &nucleos::adapter::ModelRequest) -> Result<u64> {
        Ok(1)
    }

    async fn invoke(
        &self,
        _request: nucleos::adapter::ModelRequest,
    ) -> Result<nucleos::adapter::ModelStream> {
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(ModelDelta::Text {
                part_index: 0,
                text: "partial".to_owned(),
            }),
            Err(Error::unavailable("dropped mid-stream")),
        ])))
    }
}

/// Denies tools, allows model calls: the case where the spend is fine and the action is not.
#[derive(Debug)]
struct DenyToolsOnly;

#[async_trait]
impl Policy for DenyToolsOnly {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        match request.subject {
            Gated::Tool { call } => Verdict::Deny {
                code: ErrorCode::Denied,
                reason: format!("{} is not available", call.name),
            },
            Gated::Model { .. } => Verdict::Allow,
        }
    }
}

#[derive(Debug)]
struct AskFirst;

#[async_trait]
impl Policy for AskFirst {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict {
        match request.subject {
            Gated::Tool { .. } => Verdict::AskHuman {
                question: "may I read the file?".to_owned(),
                options: vec![
                    PermissionOption {
                        id: "allow".to_owned(),
                        kind: PermissionKind::AllowOnce,
                        label: "allow".to_owned(),
                    },
                    PermissionOption {
                        id: "reject".to_owned(),
                        kind: PermissionKind::Reject,
                        label: "no".to_owned(),
                    },
                ],
            },
            Gated::Model { .. } => Verdict::Allow,
        }
    }
}
