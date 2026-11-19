//! A refusal that has already cost something is not a refusal.
//!
//! Every preflight check exists to stop a request before it is paid for. The tests here assert that
//! by counting: a model whose `count_tokens`, `prepare` and `invoke` are instrumented must show zero
//! on all three when the request is refused. Asserting only the error code cannot tell "refused
//! before the call" from "called, and then refused for the same reason".
//!
//! It also covers the edges of the byte ceiling and the places a model can be registered, because a
//! declaration is only enforced if every door into the registry checks it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Model, ModelRequest, ModelStream, Store};
use nucleos::contract::{
    Capability, Content, ErrorCode, ImageSource, Message, Part, PartKind, PartKindName, Principal,
    Role, Routing, SessionId, ToolChoice,
};
use nucleos::kernel::{InferenceOverrides, Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// A model that counts how far each call got
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Reached {
    counted: AtomicUsize,
    prepared: AtomicUsize,
    invoked: AtomicUsize,
}

impl Reached {
    fn totals(&self) -> (usize, usize, usize) {
        (
            self.counted.load(Ordering::SeqCst),
            self.prepared.load(Ordering::SeqCst),
            self.invoked.load(Ordering::SeqCst),
        )
    }
}

struct Counted {
    capability: Capability,
    reached: Arc<Reached>,
}

#[async_trait]
impl Model for Counted {
    fn capability(&self) -> Capability {
        self.capability.clone()
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        self.reached.counted.fetch_add(1, Ordering::SeqCst);
        Ok(request.text_len().div_ceil(4).max(1))
    }

    fn prepare(&self, request: ModelRequest) -> nucleos::error::Result<nucleos::adapter::Prepared> {
        self.reached.prepared.fetch_add(1, Ordering::SeqCst);
        let plan = nucleos::adapter::Plan::assume_all(&request);
        Ok(nucleos::adapter::Prepared::new(plan, request))
    }

    async fn invoke(
        &self,
        _prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<ModelStream> {
        self.reached.invoked.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(tokio_stream::iter(vec![Ok(
            nucleos::adapter::ModelDelta::Finished {
                reason: nucleos::adapter::FinishReason::Stop,
            },
        )])))
    }
}

fn capability() -> Capability {
    Capability {
        inputs: Vec::new(),
        outputs: Vec::new(),
        tool_calling: true,
        tool_choice_required: true,
        structured_output: true,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: Some(4),
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: Some(1_000_000),
        ..SessionSpec::default()
    }
}

fn build(model: Arc<Counted>, tools: bool) -> Kernel {
    let mut builder = Kernel::builder()
        .model("primary", model as Arc<dyn Model>)
        .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        });
    if tools {
        builder = builder.tool(Arc::new(nucleos_testkit::FakeTool::new("read_file", "hi")));
    }
    builder.build().unwrap()
}

fn counted(capability: Capability) -> (Arc<Counted>, Arc<Reached>) {
    let reached = Arc::new(Reached::default());
    let model = Arc::new(Counted {
        capability,
        reached: Arc::clone(&reached),
    });
    (model, reached)
}

fn user_text(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: Content::new([Part::new(PartKind::Text(nucleos::contract::TextPart {
            text: text.to_owned(),
        }))]),
    }]
}

fn user_image(bytes: Vec<u8>) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: Content::new([Part::image(ImageSource::data(bytes, "image/png").unwrap())]),
    }]
}

/// Nothing is counted, compiled or sent: the request contradicts itself before a model is consulted.
#[tokio::test]
async fn an_incoherent_request_never_reaches_the_model() {
    let (model, reached) = counted(capability());
    let kernel = build(Arc::clone(&model), true);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    // Non-text output carrying tools.
    let error = kernel
        .inference_with(
            &mut session,
            &user_text("hi"),
            InferenceOverrides {
                output: nucleos::contract::Output::Image,
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);

    // A forced choice with nothing offered.
    let error = kernel
        .inference_with(
            &mut session,
            &user_text("hi"),
            InferenceOverrides {
                tools: Some(Vec::new()),
                params: Some(nucleos::contract::GenerateParams {
                    tool_choice: ToolChoice::Required,
                    ..Default::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);

    assert_eq!(
        reached.totals(),
        (0, 0, 0),
        "a self-contradicting request cost a token count, a compilation or a call"
    );
}

/// The byte ceiling is enforced before the token count, which is the first step that costs an
/// adapter call.
#[tokio::test]
async fn an_oversized_inline_part_is_refused_before_counting() {
    let (model, reached) = counted(capability());
    let kernel = build(Arc::clone(&model), false);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let error = kernel
        .inference(&mut session, &user_image(vec![0_u8; 5]))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(
        error.reason().contains("5 bytes") && error.reason().contains("limit of 4"),
        "the refusal must name both sizes: {error}"
    );
    assert_eq!(
        reached.totals(),
        (0, 0, 0),
        "an oversized part reached the token count or the call"
    );
}

/// Exactly at the limit is allowed: the ceiling is a maximum, not a range.
#[tokio::test]
async fn an_inline_part_at_the_limit_is_accepted() {
    let (model, reached) = counted(capability());
    let kernel = build(Arc::clone(&model), false);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    // The fixture answers with no content, which the turn refuses for its own reason; the point is
    // that it got past the ceiling and therefore past the check.
    let _ = kernel
        .inference(&mut session, &user_image(vec![0_u8; 4]))
        .await;
    let (counted, prepared, _invoked) = reached.totals();
    assert_eq!(
        (counted, prepared),
        (1, 1),
        "a part exactly at the limit was refused as if it were over"
    );
}

/// A URL source carries no bytes, so it has nothing to charge against a byte ceiling.
#[tokio::test]
async fn a_referenced_source_is_not_charged_against_the_ceiling() {
    let (model, reached) = counted(capability());
    let kernel = build(Arc::clone(&model), false);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    let messages = vec![Message {
        role: Role::User,
        content: Content::new([Part::image(
            ImageSource::url("https://example.test/big.png", "image/png").unwrap(),
        )]),
    }];
    let _ = kernel.inference(&mut session, &messages).await;
    let (counted, _prepared, _invoked) = reached.totals();
    assert_eq!(
        counted, 1,
        "a referenced source was charged for bytes it does not carry"
    );
}

// ---------------------------------------------------------------------------
// Every door into the registry
// ---------------------------------------------------------------------------

/// A model whose declaration the kernel would refuse on every call cannot be registered.
#[test]
fn building_a_kernel_refuses_an_uncallable_declaration() {
    let mut capability = capability();
    // A tool call is not an output modality, and a nonzero window does not save it.
    capability.outputs = vec![PartKindName::ToolCall];
    let (model, _) = counted(capability);
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let error = Kernel::builder()
        .model("primary", model as Arc<dyn Model>)
        .store(store as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(
        error.reason().contains("primary") && error.reason().contains("output modality"),
        "{error}"
    );
}

/// `add_model` is the other door, and it checks the same thing before registering.
#[tokio::test]
async fn adding_a_model_refuses_a_bad_declaration_and_registers_nothing() {
    let (model, _) = counted(capability());
    let kernel = build(Arc::clone(&model), false);

    let mut broken = capability();
    broken.inputs = vec![PartKindName::Text, PartKindName::Text];
    let (bad, _) = counted(broken);

    let error = kernel
        .add_model("second", bad as Arc<dyn Model>)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("twice"), "{error}");

    // Not registered, so a route naming it fails at resolution rather than at the first turn.
    assert!(
        !kernel.models().iter().any(|name| name == "second"),
        "a rejected model was still registered: {:?}",
        kernel.models()
    );
}

/// The input list has its own duplicate rule, and it is not the output one.
#[test]
fn duplicate_input_kinds_are_refused() {
    let mut capability = capability();
    capability.inputs = vec![PartKindName::Text, PartKindName::Text];
    let error = capability.validate().unwrap_err();
    assert!(error.contains("declared twice"), "{error}");
    assert!(error.contains("Text"), "{error}");
}
