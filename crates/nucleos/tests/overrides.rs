//! A per-call patch over the session's configuration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{
    Capability, ErrorCode, GenerateParams, Hints, Principal, Routing, ServiceTier, SessionId,
    ToolSpec, Verbosity,
};
use nucleos::kernel::{InferenceOverrides, Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeTool, FixedClock};
use std::sync::{Arc, Mutex};

/// Records the last request it was given, so a test can see what actually crossed the seam.
#[derive(Clone, Default)]
struct Recorder {
    last: Arc<Mutex<Option<ModelRequest>>>,
}

impl Recorder {
    fn last(&self) -> ModelRequest {
        self.last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .expect("the model was called")
    }
}

#[async_trait]
impl Model for Recorder {
    fn capability(&self) -> Capability {
        Capability {
            inputs: Vec::new(),
            outputs: Vec::new(),
            tool_calling: true,
            tool_choice_required: true,
            structured_output: true,
            context_window: 128_000,
            max_output_tokens: 8_192,
            max_input_bytes: None,
        }
    }

    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(
        &self,
        prepared: nucleos::adapter::Prepared,
    ) -> nucleos::error::Result<ModelStream> {
        *self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = prepared.request().cloned();
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

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel(model: Arc<dyn Model>) -> Kernel {
    Kernel::builder()
        .model("primary", model)
        .tool(Arc::new(FakeTool::new("read_file", "hi")))
        .tool(Arc::new(FakeTool::new("write_file", "hi")))
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

/// A kernel with no tools at all, for a model that declares it accepts none.
fn kernel_without_tools(model: Arc<dyn Model>) -> Kernel {
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

/// A per-call patch changes only what it names; everything else is inherited from the session.
#[tokio::test]
async fn a_call_patches_system_params_and_hints_without_touching_the_session() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                config: nucleos::contract::ConfigPatch {
                    system: Some(Some("session prompt".to_owned())),
                    params: Some(GenerateParams {
                        temperature: Some(0.2),
                        ..GenerateParams::default()
                    }),
                    hints: Some(Hints {
                        verbosity: Some(Verbosity::Low),
                        service_tier: Some(ServiceTier::Flex),
                        ..Hints::default()
                    }),
                    ..nucleos::contract::ConfigPatch::default()
                },
                ..spec()
            },
        )
        .await
        .unwrap();

    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                system: Some("call prompt".to_owned()),
                params: Some(GenerateParams {
                    temperature: Some(0.9),
                    ..GenerateParams::default()
                }),
                hints: Some(Hints {
                    verbosity: Some(Verbosity::High),
                    ..Hints::default()
                }),
                tools: None,
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap();

    let last = recorder.last();
    assert_eq!(last.system.as_deref(), Some("call prompt"));
    assert_eq!(last.params.temperature, Some(0.9));
    assert_eq!(last.hints.verbosity, Some(Verbosity::High));
    assert_eq!(
        last.hints.service_tier,
        Some(ServiceTier::Flex),
        "the session's other hint was inherited"
    );
    assert_eq!(
        session.params().temperature,
        Some(0.2),
        "a per-call override must not rewrite the session's defaults"
    );
    assert_eq!(session.system(), Some("session prompt"));
}

/// No patch means the session's configuration is what crosses the seam.
#[tokio::test]
async fn no_patch_inherits_the_session() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(
            &principal(),
            id.clone(),
            SessionSpec {
                config: nucleos::contract::ConfigPatch {
                    system: Some(Some("session prompt".to_owned())),
                    params: Some(GenerateParams {
                        temperature: Some(0.2),
                        ..GenerateParams::default()
                    }),
                    ..nucleos::contract::ConfigPatch::default()
                },
                ..spec()
            },
        )
        .await
        .unwrap();

    kernel.inference(&mut session, &[]).await.unwrap();
    let last = recorder.last();
    assert_eq!(last.system.as_deref(), Some("session prompt"));
    assert_eq!(last.params.temperature, Some(0.2));
}

/// A request may narrow the tool set, but it may not name a tool the session would not have shown.
#[tokio::test]
async fn tools_may_only_narrow() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let spec = |name: &str| ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        input_schema_json: "{}".to_owned(),
        isolation: None,
    };

    // A subset is fine.
    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                tools: Some(vec![spec("read_file")]),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap();

    // Anything else is refused before the turn starts, so it costs no budget and writes no turn.
    let before = session.next_seq();
    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                tools: Some(vec![spec("not-a-tool")]),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("not-a-tool"), "{error}");
    assert_eq!(
        session.next_seq(),
        before,
        "a refused override wrote a turn into the log"
    );
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

/// An extension for another provider is inert: it neither reaches the wire nor fails the call.
#[tokio::test]
async fn another_providers_extension_is_inert() {
    #[derive(Debug, Clone)]
    struct Foreign;
    impl nucleos::adapter::Extension for Foreign {
        fn provider(&self) -> &str {
            "someone-else"
        }
        fn id(&self) -> &str {
            "options"
        }
        fn active_fields(&self) -> Vec<&'static str> {
            vec!["knob"]
        }
        fn validate(&self) -> nucleos::error::Result<()> {
            Ok(())
        }
        fn clone_box(&self) -> Box<dyn nucleos::adapter::Extension> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                extensions: nucleos::adapter::ExtensionSet::new(vec![Box::new(Foreign)]),
                ..InferenceOverrides::default()
            },
        )
        .await
        .expect("an extension for another provider is not an error");

    // It reached the adapter, which is what "inert" means: the adapter leaves it alone.
    assert_eq!(recorder.last().extensions.iter().count(), 1);
}

/// An identity this build does not know is a validation error, not an ignored value.
#[test]
fn an_unregistered_extension_is_refused() {
    let registry = std::collections::BTreeMap::new();
    let error = nucleos::adapter::decode_extensions(
        &[nucleos::adapter::ExtensionEntry {
            provider: "nobody".to_owned(),
            id: "nothing".to_owned(),
            fields: serde_json::json!({}),
        }],
        &registry,
        "inference.extensions",
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(
        error.reason().contains("inference.extensions[0]"),
        "the refusal names the location: {error}"
    );
    assert!(error.reason().contains("not registered"), "{error}");
}

/// A registered decoder produces a typed extension, and a duplicate identity is refused.
#[test]
fn registered_extensions_decode_and_duplicates_are_refused() {
    let registry = test_registry();
    let set = nucleos::adapter::decode_extensions(
        &[nucleos::adapter::ExtensionEntry {
            provider: "test".to_owned(),
            id: "options".to_owned(),
            fields: serde_json::json!({"knob": "value"}),
        }],
        &registry,
        "inference.extensions",
    )
    .unwrap();
    assert_eq!(
        set.active_fields()
            .first()
            .map(std::string::ToString::to_string)
            .as_deref(),
        Some("extension.test.options.knob")
    );

    let error = nucleos::adapter::decode_extensions(
        &[
            nucleos::adapter::ExtensionEntry {
                provider: "test".to_owned(),
                id: "options".to_owned(),
                fields: serde_json::json!({"knob": "a"}),
            },
            nucleos::adapter::ExtensionEntry {
                provider: "test".to_owned(),
                id: "options".to_owned(),
                fields: serde_json::json!({"knob": "b"}),
            },
        ],
        &registry,
        "inference.extensions",
    )
    .unwrap_err();
    assert!(error.reason().contains("duplicate"), "{error}");
}

/// An extension that sets nothing is a configuration mistake, not a no-op.
#[test]
fn an_extension_that_sets_nothing_is_refused() {
    let error = nucleos::adapter::decode_extensions(
        &[nucleos::adapter::ExtensionEntry {
            provider: "test".to_owned(),
            id: "empty".to_owned(),
            fields: serde_json::json!({}),
        }],
        &test_registry(),
        "inference.extensions",
    )
    .unwrap_err();
    assert!(error.reason().contains("sets no options"), "{error}");
}

/// A minimal extension, so the registry tests do not need a provider crate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TestOptions {
    knob: String,
}

impl nucleos::adapter::Extension for TestOptions {
    fn provider(&self) -> &str {
        "test"
    }
    fn id(&self) -> &str {
        "options"
    }
    fn active_fields(&self) -> Vec<&'static str> {
        vec!["knob"]
    }
    fn validate(&self) -> nucleos::error::Result<()> {
        Ok(())
    }
    fn clone_box(&self) -> Box<dyn nucleos::adapter::Extension> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// An extension that sets no options, for the emptiness check.
#[derive(Debug, Clone)]
struct EmptyOptions;

impl nucleos::adapter::Extension for EmptyOptions {
    fn provider(&self) -> &str {
        "test"
    }
    fn id(&self) -> &str {
        "empty"
    }
    fn active_fields(&self) -> Vec<&'static str> {
        Vec::new()
    }
    fn validate(&self) -> nucleos::error::Result<()> {
        Ok(())
    }
    fn clone_box(&self) -> Box<dyn nucleos::adapter::Extension> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn test_registry() -> std::collections::BTreeMap<String, nucleos::adapter::ExtensionDecoder> {
    let mut registry = std::collections::BTreeMap::new();
    registry.insert(
        "test/options".to_owned(),
        (|fields: &serde_json::Value| {
            let options: TestOptions = serde_json::from_value(fields.clone())
                .map_err(|error| nucleos::error::Error::invalid(error.to_string()))?;
            Ok(Box::new(options) as Box<dyn nucleos::adapter::Extension>)
        }) as nucleos::adapter::ExtensionDecoder,
    );
    registry.insert(
        "test/empty".to_owned(),
        (|_: &serde_json::Value| Ok(Box::new(EmptyOptions) as Box<dyn nucleos::adapter::Extension>))
            as nucleos::adapter::ExtensionDecoder,
    );
    registry
}

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

/// A model that declares what it produces refuses an output it does not declare — before the call.
#[tokio::test]
async fn an_output_the_model_does_not_declare_is_refused_before_the_call() {
    #[derive(Debug, Clone, Default)]
    struct ImagesOnly {
        called: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Model for ImagesOnly {
        fn capability(&self) -> Capability {
            Capability {
                inputs: vec![nucleos::contract::PartKindName::Text],
                outputs: vec![nucleos::contract::PartKindName::Image],
                tool_calling: false,
                tool_choice_required: false,
                structured_output: false,
                context_window: 128_000,
                max_output_tokens: 4_096,
                max_input_bytes: None,
            }
        }
        fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
            Ok(request.text_len().div_ceil(4).max(1))
        }
        async fn invoke(
            &self,
            _prepared: nucleos::adapter::Prepared,
        ) -> nucleos::error::Result<ModelStream> {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            // It answers with what it declared: a model that produces images and finishes with an
            // empty message is not a successful turn.
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(ModelDelta::PartReplace {
                    part_index: 0,
                    part: nucleos::contract::Part::image(
                        nucleos::contract::ImageSource::url(
                            "https://example.test/generated.png",
                            "image/png",
                        )
                        .unwrap(),
                    ),
                }),
                Ok(ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    let images = Arc::new(ImagesOnly::default());
    let kernel = kernel_without_tools(Arc::clone(&images) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Text is what this model does not produce, and the refusal happens before the call.
    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedFeature);
    assert!(error.reason().contains("Text"), "{error}");
    assert!(
        !images.called.load(std::sync::atomic::Ordering::SeqCst),
        "an undeclared output reached the model"
    );

    // The output it does declare is accepted.
    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                output: nucleos::contract::Output::Image,
                ..InferenceOverrides::default()
            },
        )
        .await
        .expect("the declared output is accepted");
    assert!(images.called.load(std::sync::atomic::Ordering::SeqCst));
}

/// A model that declares no input kinds accepts anything: undeclared is not "none".
#[tokio::test]
async fn an_undeclared_capability_filters_nothing() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(recorder.last().output, nucleos::contract::Output::Text);
}

// ---------------------------------------------------------------------------
// Part merge rules
// ---------------------------------------------------------------------------

/// Fragments append; a whole part replaces. The rule is the event's, not the payload's.
#[tokio::test]
async fn appended_fragments_join_and_a_replacement_replaces() {
    /// Streams two text fragments, an image preview, then the final image and one more fragment.
    struct Streaming;

    #[async_trait]
    impl Model for Streaming {
        fn capability(&self) -> Capability {
            Capability {
                inputs: Vec::new(),
                outputs: Vec::new(),
                tool_calling: false,
                tool_choice_required: false,
                structured_output: false,
                context_window: 128_000,
                max_output_tokens: 4_096,
                max_input_bytes: None,
            }
        }
        fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
            Ok(request.text_len().div_ceil(4).max(1))
        }
        async fn invoke(
            &self,
            _prepared: nucleos::adapter::Prepared,
        ) -> nucleos::error::Result<ModelStream> {
            let image = |uri: &str| {
                nucleos::contract::Part::image(
                    nucleos::contract::ImageSource::url(uri, "image/png").unwrap(),
                )
            };
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(ModelDelta::PartAppend {
                    part_index: 0,
                    part: nucleos::contract::Part::text("Hel"),
                }),
                Ok(ModelDelta::PartAppend {
                    part_index: 0,
                    part: nucleos::contract::Part::text("lo"),
                }),
                // A preview, then what it became: replace, not append.
                Ok(ModelDelta::PartReplace {
                    part_index: 1,
                    part: image("https://example.test/preview.png"),
                }),
                Ok(ModelDelta::PartReplace {
                    part_index: 1,
                    part: image("https://example.test/final.png"),
                }),
                Ok(ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    let kernel = kernel_without_tools(Arc::new(Streaming));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let outcome = kernel.inference(&mut session, &[]).await.unwrap();

    let parts = &outcome.message.content.parts;
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert_eq!(
        parts[0].kind,
        nucleos::contract::PartKind::Text(nucleos::contract::TextPart {
            text: "Hello".to_owned()
        })
    );
    match &parts[1].kind {
        nucleos::contract::PartKind::Image(image) => match image.source.from() {
            nucleos::contract::MediaFrom::Url { url } => assert_eq!(
                url, "https://example.test/final.png",
                "the replacement did not replace"
            ),
            other => panic!("expected a url, got {other:?}"),
        },
        other => panic!("expected the final image, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Audio is a modality, not a file
// ---------------------------------------------------------------------------

/// A model that declares audio output is satisfied by an `Audio` part — the mapping that was
/// impossible while audio was carried as `File`.
#[tokio::test]
async fn declared_audio_output_is_answered_by_an_audio_part() {
    struct Speech;

    #[async_trait]
    impl Model for Speech {
        fn capability(&self) -> Capability {
            Capability {
                inputs: vec![nucleos::contract::PartKindName::Text],
                outputs: vec![nucleos::contract::PartKindName::Audio],
                tool_calling: false,
                tool_choice_required: false,
                structured_output: false,
                context_window: 8_000,
                max_output_tokens: 1_000,
                max_input_bytes: None,
            }
        }
        fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
            Ok(request.text_len().div_ceil(4).max(1))
        }
        async fn invoke(
            &self,
            _prepared: nucleos::adapter::Prepared,
        ) -> nucleos::error::Result<ModelStream> {
            Ok(Box::pin(tokio_stream::iter(vec![
                // Two chunks of one clip: appended, not replaced.
                Ok(ModelDelta::PartAppend {
                    part_index: 0,
                    part: nucleos::contract::Part::audio(
                        nucleos::contract::AudioSource::data(b"first ".to_vec(), "audio/mpeg")
                            .unwrap(),
                    ),
                }),
                Ok(ModelDelta::PartAppend {
                    part_index: 0,
                    part: nucleos::contract::Part::audio(
                        nucleos::contract::AudioSource::data(b"second".to_vec(), "audio/mpeg")
                            .unwrap(),
                    ),
                }),
                Ok(ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    let kernel = kernel_without_tools(Arc::new(Speech));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Text is not what this model produces.
    let error = kernel.inference(&mut session, &[]).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedFeature);

    let outcome = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                output: nucleos::contract::Output::Audio,
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap();

    let parts = &outcome.message.content.parts;
    assert_eq!(
        parts.len(),
        1,
        "one clip, not one part per chunk: {parts:?}"
    );
    match &parts[0].kind {
        nucleos::contract::PartKind::Audio(audio) => {
            assert_eq!(audio.source.media_type(), "audio/mpeg");
            match audio.source.from() {
                nucleos::contract::MediaFrom::Data { data } => {
                    assert_eq!(data, b"first second", "the chunks were not appended");
                }
                other => panic!("expected inline bytes, got {other:?}"),
            }
        }
        other => panic!("expected an audio part, got {other:?}"),
    }
}

/// What answered is recorded with the turn: the provider's response and request identifiers.
#[tokio::test]
async fn a_response_carries_its_provider_metadata() {
    struct Identified;

    #[async_trait]
    impl Model for Identified {
        fn capability(&self) -> Capability {
            Capability {
                inputs: Vec::new(),
                outputs: Vec::new(),
                tool_calling: false,
                tool_choice_required: false,
                structured_output: false,
                context_window: 128_000,
                max_output_tokens: 4_096,
                max_input_bytes: None,
            }
        }
        fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
            Ok(request.text_len().div_ceil(4).max(1))
        }
        async fn invoke(
            &self,
            _prepared: nucleos::adapter::Prepared,
        ) -> nucleos::error::Result<ModelStream> {
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(ModelDelta::Metadata {
                    response_id: Some("resp-1".to_owned()),
                    request_id: Some("req-1".to_owned()),
                }),
                Ok(ModelDelta::PartAppend {
                    part_index: 0,
                    part: nucleos::contract::Part::text("hi"),
                }),
                Ok(ModelDelta::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    let kernel = kernel_without_tools(Arc::new(Identified));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let outcome = kernel.inference(&mut session, &[]).await.unwrap();

    assert_eq!(outcome.response_id.as_deref(), Some("resp-1"));
    assert_eq!(outcome.request_id.as_deref(), Some("req-1"));
    assert_eq!(outcome.message.content.text(), "hi");
}

// ---------------------------------------------------------------------------
// Requests that contradict themselves
// ---------------------------------------------------------------------------

/// An image or audio request carrying a toolset is a request that means two things.
///
/// The provider either refuses the call or silently picks one of them, so the refusal belongs here:
/// nothing is spent, and the message says which two fields conflict.
#[tokio::test]
async fn a_nontext_output_cannot_carry_tools() {
    let kernel = kernel(Arc::new(nucleos_testkit::FakeModel::text(["unused"])));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                output: nucleos::contract::Output::Image,
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("text output only"), "{error}");
}

/// Forcing a tool the request does not offer is a call the model cannot make.
#[tokio::test]
async fn a_forced_tool_must_be_in_the_prompt() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                params: Some(GenerateParams {
                    tool_choice: nucleos::contract::ToolChoice::Function {
                        name: "not_a_tool".to_owned(),
                    },
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("not_a_tool"), "{error}");
    assert!(
        recorder.last.lock().unwrap().is_none(),
        "the request reached the model"
    );

    // A forced call that *is* offered goes through, and the choice survives the merge. The tool is
    // named in the request's own list, which is the thing being checked.
    let spec = |name: &str| ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        input_schema_json: "{}".to_owned(),
        isolation: None,
    };
    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                tools: Some(vec![spec("read_file")]),
                params: Some(GenerateParams {
                    tool_choice: nucleos::contract::ToolChoice::Function {
                        name: "read_file".to_owned(),
                    },
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recorder.last().params.tool_choice,
        nucleos::contract::ToolChoice::Function {
            name: "read_file".to_owned()
        }
    );
}

/// `required` with no tools is the same contradiction, one level less specific.
#[tokio::test]
async fn a_required_tool_choice_needs_tools() {
    let kernel = kernel_without_tools(Arc::new(nucleos_testkit::FakeModel::text(["unused"])));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                params: Some(GenerateParams {
                    tool_choice: nucleos::contract::ToolChoice::Required,
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(
        error.reason().contains("requires at least one tool"),
        "{error}"
    );
}

/// A tool call is a part at its own index, not a second sequence appended after the text.
///
/// Before this, assembly emitted every content part first and then every tool call, so a turn that
/// said "before", called a tool, and said "after" was stored as "before after" followed by the call.
/// The event stream and the stored message described different turns.
#[tokio::test]
async fn a_tool_call_keeps_its_place_in_the_message() {
    let model = nucleos_testkit::FakeModel::with_script(vec![vec![
        ModelDelta::PartAppend {
            part_index: 0,
            part: nucleos::contract::Part::text("before"),
        },
        ModelDelta::ToolCallStarted {
            index: 0,
            part_index: 1,
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
        },
        ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"path\":\"a\"}".to_owned(),
        },
        ModelDelta::PartAppend {
            part_index: 2,
            part: nucleos::contract::Part::text("after"),
        },
        ModelDelta::Finished {
            reason: FinishReason::ToolCalls,
        },
    ]]);
    let kernel = kernel_without_tools(Arc::new(model));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let outcome = kernel.inference(&mut session, &[]).await.unwrap();
    let parts = &outcome.message.content.parts;
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(matches!(
        &parts[0].kind,
        nucleos::contract::PartKind::Text(text) if text.text == "before"
    ));
    assert!(matches!(
        &parts[1].kind,
        nucleos::contract::PartKind::ToolCall(call) if call.call.name == "read_file"
    ));
    assert!(matches!(
        &parts[2].kind,
        nucleos::contract::PartKind::Text(text) if text.text == "after"
    ));
}

/// The kernel fills correlation data; an adapter does not have to guess which session it is serving.
#[tokio::test]
async fn the_kernel_fills_correlation_for_the_adapter() {
    let recorder = Arc::new(Recorder::default());
    let kernel = kernel_without_tools(Arc::clone(&recorder) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    kernel.inference(&mut session, &[]).await.unwrap();
    let correlation = recorder
        .last()
        .correlation
        .expect("the request carries correlation");
    assert_eq!(correlation.session, "s1");
    assert_eq!(correlation.turn, 1);
    assert_eq!(correlation.tenant, "acme");
    assert_eq!(correlation.subject, "alice");
}

/// A token limit says nothing about a file: an inline media part has its own byte ceiling.
#[tokio::test]
async fn an_inline_media_part_over_the_byte_limit_is_refused() {
    let capability = Capability {
        inputs: vec![nucleos::contract::PartKindName::Image],
        outputs: vec![nucleos::contract::PartKindName::Text],
        tool_calling: false,
        tool_choice_required: false,
        structured_output: false,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: Some(4),
    };
    let kernel = kernel_without_tools(Arc::new(
        nucleos_testkit::FakeModel::text(["unused"]).with_capability(capability),
    ));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();
    let messages = vec![nucleos::contract::Message {
        role: nucleos::contract::Role::User,
        content: nucleos::contract::Content::new([nucleos::contract::Part::image(
            nucleos::contract::ImageSource::data(vec![0_u8; 5], "image/png").unwrap(),
        )]),
    }];

    let error = kernel.inference(&mut session, &messages).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.reason().contains("5 bytes"), "{error}");
}

/// A model that says it cannot be forced must not be sent a forced choice.
///
/// `tool_choice: required` downgraded to "maybe" is an extraction step that gets an answer it cannot
/// use, having paid for it. The declaration is the only thing that can refuse before the call, so a
/// declaration the kernel does not read is a guarantee nobody keeps.
#[tokio::test]
async fn a_forced_tool_choice_is_refused_by_a_model_that_cannot_be_forced() {
    let capability = Capability {
        inputs: Vec::new(),
        outputs: Vec::new(),
        tool_calling: true,
        // It accepts tools and calls them freely; it cannot be *made* to.
        tool_choice_required: false,
        structured_output: true,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: None,
    };
    let model = Arc::new(nucleos_testkit::FakeModel::text(["ok"]).with_capability(capability));
    let kernel = kernel(Arc::clone(&model) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    // Offered, so the request is coherent and the only question left is whether this model can be
    // forced — which it says it cannot.
    let offered = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: String::new(),
        input_schema_json: "{}".to_owned(),
        isolation: None,
    }];
    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                tools: Some(offered.clone()),
                params: Some(GenerateParams {
                    tool_choice: nucleos::contract::ToolChoice::Function {
                        name: "read_file".to_owned(),
                    },
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedFeature);
    assert!(
        error.reason().contains("cannot be made to call a tool"),
        "{error}"
    );
    assert_eq!(model.calls(), 0, "the refusal must happen before the call");

    // An unforced choice is fine: the capability only limits forcing.
    kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                tools: Some(offered),
                params: Some(GenerateParams {
                    tool_choice: nucleos::contract::ToolChoice::Auto,
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap();
}

/// A model that cannot be held to a schema must not be sent one.
#[tokio::test]
async fn a_json_schema_is_refused_by_a_model_without_structured_output() {
    let capability = Capability {
        inputs: Vec::new(),
        outputs: Vec::new(),
        tool_calling: false,
        tool_choice_required: false,
        structured_output: false,
        context_window: 128_000,
        max_output_tokens: 4_096,
        max_input_bytes: None,
    };
    let model = Arc::new(nucleos_testkit::FakeModel::text(["unused"]).with_capability(capability));
    let kernel = kernel_without_tools(Arc::clone(&model) as Arc<dyn Model>);
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let error = kernel
        .inference_with(
            &mut session,
            &[],
            InferenceOverrides {
                params: Some(GenerateParams {
                    format: nucleos::contract::TextFormat::JsonSchema {
                        name: "answer".to_owned(),
                        schema_json: "{\"type\":\"object\"}".to_owned(),
                    },
                    ..GenerateParams::default()
                }),
                ..InferenceOverrides::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedFeature);
    assert!(error.reason().contains("JSON Schema"), "{error}");
    assert_eq!(model.calls(), 0, "the refusal must happen before the call");
}

/// Assembly orders by part index, not by arrival.
///
/// The earlier ordering test fed its deltas in ascending order, so an assembler that ignored
/// `part_index` and kept arrival order would have passed it. A provider is free to finish a later part
/// first — the index is the promise, the arrival order is not.
#[tokio::test]
async fn parts_are_ordered_by_index_not_by_arrival() {
    let model = nucleos_testkit::FakeModel::with_script(vec![vec![
        // Deliberately out of order: the last part arrives first.
        ModelDelta::PartAppend {
            part_index: 2,
            part: nucleos::contract::Part::text("after"),
        },
        ModelDelta::ToolCallStarted {
            index: 0,
            part_index: 1,
            id: "call-1".to_owned(),
            name: "read_file".to_owned(),
        },
        ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{}".to_owned(),
        },
        ModelDelta::PartAppend {
            part_index: 0,
            part: nucleos::contract::Part::text("before"),
        },
        ModelDelta::Finished {
            reason: FinishReason::ToolCalls,
        },
    ]]);
    let kernel = kernel_without_tools(Arc::new(model));
    let id = SessionId::new("s1");
    let mut session = kernel.open(&principal(), id.clone(), spec()).await.unwrap();

    let outcome = kernel.inference(&mut session, &[]).await.unwrap();
    let parts = &outcome.message.content.parts;
    assert_eq!(parts.len(), 3, "{parts:?}");
    assert!(matches!(
        &parts[0].kind,
        nucleos::contract::PartKind::Text(text) if text.text == "before"
    ));
    assert!(matches!(
        &parts[1].kind,
        nucleos::contract::PartKind::ToolCall(call) if call.call.name == "read_file"
    ));
    assert!(matches!(
        &parts[2].kind,
        nucleos::contract::PartKind::Text(text) if text.text == "after"
    ));
}
