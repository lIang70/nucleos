//! The adapter's contract: it must account for exactly what the request asked for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Plan, Store,
};
use nucleos::contract::{
    Capability, ErrorCode, Field, Hint, Hints, Param, Principal, Routing, SessionId, Verbosity,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel(model: Arc<dyn Model>) -> Kernel {
    Kernel::builder()
        .model("primary", model)
        .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: Some(1_000_000),
        ..SessionSpec::default()
    }
}

/// An adapter that honours only `temperature` and says so.
struct OnlyTemperature;

#[async_trait]
impl Model for OnlyTemperature {
    fn capability(&self) -> Capability {
        Capability {
            inputs: Vec::new(),
            outputs: Vec::new(),
            tool_calling: true,
            tool_choice_required: true,
            structured_output: true,
            context_window: 128_000,
            max_output_tokens: 4_096,
            max_input_bytes: None,
        }
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    fn prepare(&self, request: ModelRequest) -> nucleos::error::Result<nucleos::adapter::Prepared> {
        Ok(nucleos::adapter::Prepared::new(
            Plan {
                honoured: vec![Field::Param(Param::Temperature)],
                degraded: Vec::new(),
            },
            request,
        ))
    }

    async fn invoke(
        &self,
        _prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<ModelStream> {
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(ModelDelta::PartAppend {
                part_index: 0,
                part: nucleos::contract::Part::text("ok".to_owned()),
            }),
            Ok(ModelDelta::Finished {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}

/// An adapter that claims a knob the request never set.
struct Inventor;

#[async_trait]
impl Model for Inventor {
    fn capability(&self) -> Capability {
        OnlyTemperature.capability()
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        OnlyTemperature.count_tokens(request)
    }

    fn prepare(&self, request: ModelRequest) -> nucleos::error::Result<nucleos::adapter::Prepared> {
        let mut plan = Plan::assume_all(&request);
        plan.honoured.push(Field::Hint(Hint::ServiceTier));
        Ok(nucleos::adapter::Prepared::new(plan, request))
    }

    async fn invoke(
        &self,
        prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<ModelStream> {
        OnlyTemperature.invoke(prepared).await
    }
}

fn request_with_a_hint() -> (Kernel, SessionId) {
    let kernel = kernel(Arc::new(OnlyTemperature));
    let id = SessionId::new("s1");
    (kernel, id)
}

/// A field the caller set and the adapter neither honoured nor explained is an adapter bug, and it
/// fails the attempt instead of quietly changing the answer.
#[tokio::test]
async fn a_field_the_adapter_did_not_account_for_fails_the_turn() {
    let (kernel, id) = request_with_a_hint();
    let session = kernel.open(&principal(), id, spec()).await.unwrap();

    // A session-level hint: `GenerateParams` alone would not trigger this, which is why the check
    // needs to see hints as well as sampling knobs.
    let mut session = session;
    kernel
        .configure(
            &mut session,
            nucleos::contract::ConfigPatch {
                hints: Some(Hints {
                    verbosity: Some(Verbosity::High),
                    ..Hints::default()
                }),
                ..nucleos::contract::ConfigPatch::default()
            },
        )
        .await
        .unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::AdapterContractViolation);
    assert!(
        error.reason().contains("hint.verbosity"),
        "the refusal names the knob: {error}"
    );
}

/// Claiming a field the request never set is the other way to be wrong, and it is caught too.
#[tokio::test]
async fn a_field_the_request_never_set_is_an_adapter_bug() {
    let kernel = kernel(Arc::new(Inventor));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id, spec()).await.unwrap();

    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::AdapterContractViolation);
    assert!(
        error.reason().contains("never set"),
        "the refusal says the adapter invented a field: {error}"
    );
}

/// The default plan claims every field, so an adapter that has not been taught to be specific still
/// works — the check says nothing about it, by design.
#[tokio::test]
async fn an_adapter_with_no_specific_plan_still_works() {
    let kernel = kernel(Arc::new(FakeModel::text(["ok"])));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id, spec()).await.unwrap();
    let outcome = kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(outcome.message.content.text(), "ok");
}
