//! The generation, embedding and judgment seams, and the struct that carries them.
//!
//! Three interfaces, one struct. A provider that cannot embed does not carry a
//! stub — its [`Provider::embedder`] is `None` — and [`Provider::validate`] makes
//! "declared but uncallable" a construction error instead of a capability a
//! caller can discover and never use.

use std::sync::Arc;

use async_trait::async_trait;
use nucleos_contract::{
    Capabilities, Error, Mode, ModelDescriptor, ModelId, Output, Result, ToolResult,
};
pub use nucleos_contract::{
    DecideRequest, Decision, DecisionResponse, EmbedRequest, EmbedResponse, GenerateResponse,
    ModelRequest, Probability, Question, QuestionKind, StreamEvent,
};

/// An in-flight generation.
///
/// `next` returns `None` at the end, after which [`Stream::result`] yields the
/// same value a non-streaming call would have returned — which is what lets a
/// caller that does not care about increments treat both paths alike.
#[async_trait]
pub trait Stream: Send {
    /// The next event, or `None` when the stream is finished.
    async fn next(&mut self) -> Option<Result<StreamEvent>>;

    /// The assembled answer. Meaningful only after `next` returned `None`.
    fn result(self: Box<Self>) -> Result<GenerateResponse>;

    /// Release whatever the stream holds.
    async fn close(self: Box<Self>);
}

/// The generation seam.
///
/// Video is unary here even when the provider runs it as a long task: the driver
/// creates the task and polls it inside `generate`, so the caller's deadline is
/// the whole budget. A driver that cannot stream an output says so with
/// [`ErrorKind::Unsupported`](nucleos_contract::ErrorKind::Unsupported) rather
/// than holding a stream open for minutes.
#[async_trait]
pub trait Generator: Send + Sync {
    /// One call, answered whole.
    async fn generate(&self, request: ModelRequest) -> Result<GenerateResponse>;

    /// One call, answered incrementally.
    async fn generate_stream(&self, request: ModelRequest) -> Result<Box<dyn Stream>>;
}

/// The embedding seam.
///
/// Not `generate` with a different output: vectors are not a message, have no
/// role and no finish reason, and a batch of documents is not a conversation.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed every document in the request.
    async fn embed(&self, request: EmbedRequest) -> Result<EmbedResponse>;
}

/// The judgment seam.
///
/// Not `generate` with a schema: a decision carries several questions in one
/// call, typed answers with confidence, and per-question refusals — none of which
/// is a message. The response shape earns the seam.
#[async_trait]
pub trait Decider: Send + Sync {
    /// Answer every question, in order.
    async fn decide(&self, request: DecideRequest) -> Result<DecisionResponse>;
}

/// The model facts an application declared.
///
/// It is how capabilities reach the runtime: without a reader, a deployment's
/// model declarations would be data with no consumer.
pub trait Catalog: Send + Sync {
    /// Every declared model.
    fn models(&self) -> Vec<ModelDescriptor>;

    /// One model's descriptor, if it was declared.
    fn describe(&self, id: &ModelId) -> Option<ModelDescriptor>;
}

/// One vendor: the seams it serves, and the line-up it declared.
///
/// A struct rather than an interface with a method per capability: adding a mode
/// is one field, and `None` is the honest answer to "does this provider do it".
#[derive(Clone)]
pub struct Provider {
    generator: Arc<dyn Generator>,
    embedder: Option<Arc<dyn Embedder>>,
    decider: Option<Arc<dyn Decider>>,
    catalog: Option<Arc<dyn Catalog>>,
}

impl Provider {
    /// The only constructor.
    ///
    /// A struct literal cannot exist, so "a provider with no generator" is not a
    /// value that fails `validate` — it is a value that cannot be written.
    #[must_use]
    pub fn new(generator: Arc<dyn Generator>) -> Self {
        Self {
            generator,
            embedder: None,
            decider: None,
            catalog: None,
        }
    }

    /// Serve embeddings.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Serve judgments.
    #[must_use]
    pub fn with_decider(mut self, decider: Arc<dyn Decider>) -> Self {
        self.decider = Some(decider);
        self
    }

    /// Declare the model line-up.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Arc<dyn Catalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Generation, unchecked.
    ///
    /// For an adapter's own tests. A runtime calls [`Provider::generate`] or
    /// [`Provider::generate_stream`] instead, because those check the accounting before a record is
    /// written from it.
    #[must_use]
    pub fn generator(&self) -> &Arc<dyn Generator> {
        &self.generator
    }

    /// One call, answered whole, with the accounting checked.
    ///
    /// # Errors
    ///
    /// The provider failed, or reported a breakdown that contradicts its own total.
    pub async fn generate(&self, request: ModelRequest) -> Result<GenerateResponse> {
        let response = self.generator.generate(request).await?;
        response.validate()?;
        Ok(response)
    }

    /// One call, answered incrementally, with the assembled result checked.
    ///
    /// # Errors
    ///
    /// The provider failed to start, or does not stream.
    pub async fn generate_stream(&self, request: ModelRequest) -> Result<Box<dyn Stream>> {
        let stream = self.generator.generate_stream(request).await?;
        Ok(Box::new(CheckedStream { inner: stream }))
    }

    /// Embed, with the accounting and the vector count checked.
    ///
    /// # Errors
    ///
    /// The provider serves no embeddings, failed, or answered a different number of documents than
    /// it was given.
    pub async fn embed(&self, request: EmbedRequest) -> Result<EmbedResponse> {
        let expected = request.input.len();
        let response = self
            .embedder
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    nucleos_contract::ErrorKind::Unsupported,
                    "this provider serves no embeddings",
                )
            })?
            .embed(request)
            .await?;
        response.validate(expected)?;
        Ok(response)
    }

    /// Decide, with the accounting and the answer count checked.
    ///
    /// # Errors
    ///
    /// The provider serves no decisions, failed, or answered a different number of questions than it
    /// was asked.
    pub async fn decide(&self, request: DecideRequest) -> Result<DecisionResponse> {
        let expected = request.questions.len();
        let response = self
            .decider
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    nucleos_contract::ErrorKind::Unsupported,
                    "this provider serves no decisions",
                )
            })?
            .decide(request)
            .await?;
        response.validate(expected)?;
        Ok(response)
    }

    /// Embeddings, when the vendor serves them.
    #[must_use]
    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    /// Judgments, when the vendor serves them.
    #[must_use]
    pub fn decider(&self) -> Option<&Arc<dyn Decider>> {
        self.decider.as_ref()
    }

    /// The declared line-up, when the deployment named models.
    #[must_use]
    pub fn catalog(&self) -> Option<&Arc<dyn Catalog>> {
        self.catalog.as_ref()
    }

    /// The one place that can answer "declared but uncallable".
    ///
    /// A model that declares an embedding width while no embedder is bound — or
    /// decision support while no decider is bound — is a capability a caller can
    /// discover and never use. Failing here turns that into a deployment error.
    pub fn validate(&self) -> Result<()> {
        let Some(catalog) = &self.catalog else {
            return Ok(());
        };
        for descriptor in catalog.models() {
            let capabilities: &Capabilities = &descriptor.capabilities;
            if capabilities.can(Mode::Embed) && self.embedder.is_none() {
                return Err(Error::new(
                    nucleos_contract::ErrorKind::Unsupported,
                    format!(
                        "model {:?} declares the embed mode but this provider serves no embeddings",
                        descriptor.id.as_str()
                    ),
                ));
            }
            if capabilities.can(Mode::Decide) && self.decider.is_none() {
                return Err(Error::new(
                    nucleos_contract::ErrorKind::Unsupported,
                    format!(
                        "model {:?} declares the decide mode but this provider serves no decisions",
                        descriptor.id.as_str()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Whether a model may be asked for an output.
    pub fn serves(&self, model: &ModelId, output: Output) -> Result<()> {
        let Some(catalog) = &self.catalog else {
            return Ok(());
        };
        let Some(descriptor) = catalog.describe(model) else {
            return Err(Error::new(
                nucleos_contract::ErrorKind::UnknownModel,
                format!("no model named {:?}", model.as_str()),
            ));
        };
        if !descriptor.capabilities.can(output.mode()) {
            return Err(Error::new(
                nucleos_contract::ErrorKind::Unsupported,
                format!(
                    "model {:?} cannot produce {:?}",
                    model.as_str(),
                    output.mode()
                ),
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("embedder", &self.embedder.is_some())
            .field("decider", &self.decider.is_some())
            .field("catalog", &self.catalog.is_some())
            .finish_non_exhaustive()
    }
}

/// A stream whose assembled response is checked before it is handed over.
///
/// The ceiling on token counts is only worth having at the door a runtime passes through, and a
/// streaming call has no such door until the stream ends — so the wrapper checks in `result` instead.
struct CheckedStream {
    inner: Box<dyn Stream>,
}

#[async_trait]
impl Stream for CheckedStream {
    async fn next(&mut self) -> Option<Result<StreamEvent>> {
        self.inner.next().await
    }

    fn result(self: Box<Self>) -> Result<GenerateResponse> {
        let response = self.inner.result()?;
        response.validate()?;
        Ok(response)
    }

    async fn close(self: Box<Self>) {
        self.inner.close().await;
    }
}

/// A stream that yields nothing: the shape a generator returns when a call has
/// no incremental form.
pub async fn once(event: StreamEvent) -> Result<Box<dyn Stream>> {
    Ok(Box::new(Once { event: Some(event) }))
}

struct Once {
    event: Option<StreamEvent>,
}

#[async_trait]
impl Stream for Once {
    async fn next(&mut self) -> Option<Result<StreamEvent>> {
        self.event.take().map(Ok)
    }

    fn result(self: Box<Self>) -> Result<GenerateResponse> {
        Err(Error::internal(
            "a single-event stream has no assembled response",
        ))
    }

    async fn close(self: Box<Self>) {}
}

/// A tool result a stream can carry, re-exported so an adapter need not depend on
/// the content module twice.
pub type StreamedToolResult = ToolResult;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use nucleos_contract::InputTokenUsage;
    use nucleos_contract::{Capabilities, FinishReason, Message, Mode, ModelId, Outcome, Usage};
    use std::sync::Arc;

    struct Silent;

    #[async_trait]
    impl Generator for Silent {
        async fn generate(&self, _request: ModelRequest) -> Result<GenerateResponse> {
            Err(Error::internal("no"))
        }
        async fn generate_stream(&self, _request: ModelRequest) -> Result<Box<dyn Stream>> {
            Err(Error::internal("no"))
        }
    }

    struct Declared(Vec<ModelDescriptor>);

    impl Catalog for Declared {
        fn models(&self) -> Vec<ModelDescriptor> {
            self.0.clone()
        }
        fn describe(&self, id: &ModelId) -> Option<ModelDescriptor> {
            self.0.iter().find(|model| model.id == *id).cloned()
        }
    }

    fn descriptor(id: &str, capabilities: Capabilities) -> ModelDescriptor {
        ModelDescriptor {
            id: ModelId::new(id).unwrap(),
            label: None,
            capabilities,
            limits: Default::default(),
            lifecycle: Default::default(),
        }
    }

    fn declared(models: Vec<ModelDescriptor>) -> Arc<dyn Catalog> {
        Arc::new(Declared(models))
    }

    #[test]
    fn a_provider_without_a_catalog_accepts_what_it_is_given() {
        let provider = Provider::new(Arc::new(Silent));
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn a_declared_embed_mode_without_an_embedder_is_refused() {
        let provider = Provider::new(Arc::new(Silent)).with_catalog(declared(vec![descriptor(
            "embed-1",
            Capabilities {
                embedding_dimensions: Some(1024),
                ..Capabilities::default()
            },
        )]));
        let error = provider.validate().unwrap_err();
        assert_eq!(error.kind, nucleos_contract::ErrorKind::Unsupported);
        assert!(error.message.contains("embed"), "{error:?}");
    }

    #[test]
    fn a_declared_decide_mode_without_a_decider_is_refused() {
        let provider = Provider::new(Arc::new(Silent)).with_catalog(declared(vec![descriptor(
            "judge-1",
            Capabilities {
                modes: vec![Mode::Decide],
                ..Capabilities::default()
            },
        )]));
        assert!(provider.validate().is_err());
    }

    #[test]
    fn a_declared_text_model_needs_nothing_extra() {
        let provider = Provider::new(Arc::new(Silent)).with_catalog(declared(vec![descriptor(
            "chat-1",
            Capabilities::default(),
        )]));
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn serving_checks_the_output_against_the_declaration() {
        let model = ModelId::new("chat-1").unwrap();
        let provider = Provider::new(Arc::new(Silent)).with_catalog(declared(vec![descriptor(
            "chat-1",
            Capabilities::default(),
        )]));
        assert!(provider.serves(&model, Output::Text).is_ok());
        let error = provider.serves(&model, Output::Image).unwrap_err();
        assert_eq!(error.kind, nucleos_contract::ErrorKind::Unsupported);
    }

    #[test]
    fn an_undeclared_model_is_unknown_rather_than_unsupported() {
        let provider = Provider::new(Arc::new(Silent)).with_catalog(declared(Vec::new()));
        let error = provider
            .serves(&ModelId::new("nope").unwrap(), Output::Text)
            .unwrap_err();
        assert_eq!(error.kind, nucleos_contract::ErrorKind::UnknownModel);
    }

    struct Contradictory;

    #[async_trait]
    impl Generator for Contradictory {
        async fn generate(&self, _request: ModelRequest) -> Result<GenerateResponse> {
            Ok(GenerateResponse {
                model: nucleos_contract::ModelRef::new("openai/gpt-5").unwrap(),
                message: Message::assistant("hi"),
                finish: FinishReason::Stop,
                outcome: Outcome {
                    usage: Usage {
                        input_tokens: 1,
                        input: InputTokenUsage {
                            cache_read_tokens: Some(10),
                            ..InputTokenUsage::default()
                        },
                        ..Usage::default()
                    },
                    ..Outcome::default()
                },
                warnings: Vec::new(),
            })
        }

        async fn generate_stream(&self, _request: ModelRequest) -> Result<Box<dyn Stream>> {
            Err(Error::internal("no"))
        }
    }

    #[tokio::test]
    async fn a_contradictory_breakdown_never_leaves_the_checked_path() {
        let provider = Provider::new(Arc::new(Contradictory));
        let error = provider
            .generate(ModelRequest::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, nucleos_contract::ErrorKind::Invalid);
        assert!(
            error.message.contains("cache"),
            "the failure must name the contradiction: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_provider_without_an_embedder_refuses_rather_than_pretending() {
        let provider = Provider::new(Arc::new(Silent));
        let error = provider.embed(EmbedRequest::default()).await.unwrap_err();
        assert_eq!(error.kind, nucleos_contract::ErrorKind::Unsupported);
    }
}
