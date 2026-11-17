//! The adapter: configuration, and the `Model` implementation over `wire` and `decode`.

use async_trait::async_trait;
use futures_util::StreamExt;
use nucleos::adapter::{FinishReason, Model, ModelDelta, ModelRequest, ModelStream};
use nucleos::contract::Capability;
use nucleos::error::{Error, Result};
use std::time::Duration;

/// How to reach one model: the connection, and nothing about what the model can do.
///
/// A url, a credential, a provider model name and a timeout are connection facts. Whether the model
/// accepts strict schemas, produces images, or accepts a reasoning effort is a fact *about the model*,
/// and belongs in the daemon's model catalogue rather than in a connection config.
#[derive(Debug, Clone)]
pub struct Config {
    /// The full URL of the responses endpoint.
    ///
    /// Full rather than a base: compatible endpoints disagree about where the path goes, and
    /// guessing for them is how a working config breaks on the next one.
    pub url: String,
    /// A bearer token, when the endpoint wants one. `None` is a local server.
    pub api_key: Option<String>,
    /// The provider's model identifier to send on the wire.
    pub model: String,
    /// How long to wait for the response headers.
    pub timeout: Option<Duration>,
    /// The image generation endpoint, when the deployment serves `Output::Image`.
    ///
    /// A separate URL rather than a path derived from [`Config::url`]: compatible endpoints disagree
    /// about where the paths go, and guessing for them is how a working config breaks on the next
    /// one. `None` means this model serves no images, and a request for one is refused before it is
    /// sent rather than posted to a URL that happens to look right.
    pub images_url: Option<String>,
    /// The speech synthesis endpoint, when the deployment serves `Output::Audio`. Same rule.
    pub speech_url: Option<String>,
}

/// What the daemon's model catalogue says about one OpenAI model.
///
/// This is deliberately *not* called `Config`: it is the answer to "what is this model" — its declared
/// capabilities, and the OpenAI-specific defaults a deployment may set for a call. Those defaults can
/// be overridden per call through an `openai.generation` extension; the connection config stays out of
/// that business.
#[derive(Debug, Clone)]
pub struct OpenAiSettings {
    /// Declared capabilities: tools, structured output, streaming, and the token limits.
    pub capability: nucleos::contract::Capability,
    /// Whether tool schemas are declared strict.
    ///
    /// Only for schemas that are eligible: `additionalProperties: false`, and every property listed
    /// as required. The provider rejects `strict: true` otherwise, and it rejects the whole call.
    pub strict_tools: bool,
    /// Whether this model accepts `reasoning.effort`.
    ///
    /// Asked of the catalogue rather than probed, because the alternative is a 400 that kills a call
    /// over a hint. When false, a requested effort is reported as dropped and the call proceeds.
    pub supports_reasoning: bool,
    /// The deployment's default hints. A call's hints override them field by field.
    pub options: nucleos::contract::Hints,
    /// The provider's prefix-cache key.
    ///
    /// Derived by the host, usually from the session id, so that a session's turns hit the same
    /// cache. It is not the application's to choose: a wrong key costs money and changes nothing the
    /// application can observe.
    pub prompt_cache_key: Option<String>,
    /// Extra data to ask the provider for.
    pub include: Vec<crate::wire::Include>,
    /// A privacy-preserving identifier for the end user, derived from the principal.
    pub safety_identifier: Option<String>,
}

impl OpenAiSettings {
    /// A capability set that assumes nothing surprising.
    #[must_use]
    pub fn capabilities(
        context_window: u32,
        max_output_tokens: u32,
    ) -> nucleos::contract::Capability {
        nucleos::contract::Capability {
            inputs: Vec::new(),
            outputs: Vec::new(),
            tool_calling: true,
            tool_choice_required: true,
            structured_output: true,
            context_window,
            max_output_tokens,
            max_input_bytes: None,
        }
    }
}

impl Default for OpenAiSettings {
    fn default() -> Self {
        Self {
            capability: Self::capabilities(128_000, 8_192),
            strict_tools: false,
            supports_reasoning: false,
            options: nucleos::contract::Hints::default(),
            prompt_cache_key: None,
            include: Vec::new(),
            safety_identifier: None,
        }
    }
}

/// One model behind the kernel's [`Model`] contract.
#[derive(Clone)]
pub struct OpenAi {
    config: Config,
    settings: OpenAiSettings,
    http: reqwest::Client,
}

impl std::fmt::Debug for OpenAi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is a secret; the model and the URL are what a reader wants.
        f.debug_struct("OpenAi")
            .field("url", &self.config.url)
            .field("model", &self.config.model)
            .field("supports_reasoning", &self.settings.supports_reasoning)
            .finish_non_exhaustive()
    }
}

impl OpenAi {
    /// Build an adapter from a connection config and the catalogue's settings.
    pub fn new(config: Config, settings: OpenAiSettings) -> Result<Self> {
        if config.url.trim().is_empty() {
            return Err(Error::invalid("a model needs a url"));
        }
        if config.model.trim().is_empty() {
            return Err(Error::invalid("a model needs a name"));
        }
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = config.timeout {
            builder = builder.timeout(timeout);
        }
        let http = builder
            .build()
            .map_err(|error| Error::invalid(format!("cannot build an http client: {error}")))?;
        Ok(Self {
            config,
            settings,
            http,
        })
    }
}

impl OpenAi {
    /// The URL for one surface, or the reason this deployment cannot serve it.
    fn endpoint_for(&self, endpoint: crate::wire::Endpoint) -> Result<&str> {
        use crate::wire::Endpoint;
        match endpoint {
            Endpoint::Responses => Ok(&self.config.url),
            Endpoint::Images => self.config.images_url.as_deref().ok_or_else(|| {
                nucleos::error::Error::new(
                    nucleos::contract::ErrorCode::UnsupportedFeature,
                    "this model serves no images: the deployment configured no image endpoint",
                )
            }),
            Endpoint::Speech => self.config.speech_url.as_deref().ok_or_else(|| {
                nucleos::error::Error::new(
                    nucleos::contract::ErrorCode::UnsupportedFeature,
                    "this model serves no speech: the deployment configured no speech endpoint",
                )
            }),
        }
    }

    /// The deployment's settings with this call's correlation and `openai.generation` options
    /// applied.
    ///
    /// Precedence is deployment value, then per-call extension, then a value derived from the
    /// session: an explicit configuration must win over a guess, and a guess is better than a
    /// process-wide default. `include` *replaces* rather than appends — a caller overriding one list
    /// must not end up with two copies of it. Everything else stays the catalogue's fact, not a
    /// per-call choice.
    fn effective_settings<'a>(
        &'a self,
        request: &'a ModelRequest,
        derived_cache_key: Option<&'a str>,
        derived_safety_identifier: Option<&'a str>,
    ) -> crate::wire::Settings<'a> {
        let mut settings = self.settings();
        // Only fill a gap. A deployment that chose a key, or a caller who overrode it, has said
        // something more specific than "use the session".
        if settings.cache_key.is_none() {
            settings.cache_key = derived_cache_key;
        }
        if settings.safety_identifier.is_none() {
            settings.safety_identifier = derived_safety_identifier;
        }
        if let Some(options) = request
            .extensions
            .for_provider("openai")
            .find_map(|extension| {
                extension
                    .as_any()
                    .downcast_ref::<crate::extension::GenerationOptions>()
            })
        {
            if options.prompt_cache_key.is_some() {
                settings.cache_key = options.prompt_cache_key.as_deref();
            }
            if options.safety_identifier.is_some() {
                settings.safety_identifier = options.safety_identifier.as_deref();
            }
            if !options.include.is_empty() {
                settings.include = &options.include;
            }
        }
        settings
    }

    /// The knobs this endpoint is known to support, as the wire encoder needs them.
    fn settings(&self) -> crate::wire::Settings<'_> {
        crate::wire::Settings {
            model: &self.config.model,
            supports_reasoning: self.settings.supports_reasoning,
            strict_tools: self.settings.strict_tools,
            options: &self.settings.options,
            cache_key: self.settings.prompt_cache_key.as_deref(),
            include: &self.settings.include,
            safety_identifier: self.settings.safety_identifier.as_deref(),
        }
    }
}

#[async_trait]
impl Model for OpenAi {
    fn provider_id(&self) -> &'static str {
        "openai"
    }

    fn capability(&self) -> Capability {
        self.settings.capability.clone()
    }

    /// A conservative estimate, and it says so by being one.
    ///
    /// This adapter carries no tokenizer. Four characters per token is the usual rule of thumb and
    /// it under-estimates for code and for CJK — which is the safe direction: the kernel uses it to
    /// refuse a turn that will not fit, and refusing early is recoverable while overflowing is not.
    /// Compile the request, once, and hand the compiled body to the call.
    ///
    /// The plan and the body come out of the same `encode`, which is the whole point: a plan
    /// derived from one compilation while the call sends another is how a per-call option gets
    /// reported as honoured and then dropped. Here the body that was checked is the body that is
    /// sent, and the degradations are the same list the encoder produced.
    ///
    /// `effective_settings` — the deployment defaults with this call's `openai.generation` options
    /// applied — is the only settings this path may use. `settings()` alone would silently ignore
    /// every per-call option.
    fn prepare(&self, request: ModelRequest) -> Result<nucleos::adapter::Prepared> {
        // Derived per call, not per process: the cache key follows the session so its turns share a
        // prefix cache, and the safety identifier follows the subject without sending the subject
        // itself. Both are fallbacks; an explicit setting or extension wins.
        let derived_cache_key = request.correlation.as_ref().map(|correlation| {
            if correlation.tenant.is_empty() {
                correlation.session.clone()
            } else {
                format!("{}/{}", correlation.tenant, correlation.session)
            }
        });
        let derived_safety_identifier = request.correlation.as_ref().map(|correlation| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(
                format!("{}\0{}", correlation.tenant, correlation.subject).as_bytes(),
            );
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        });
        let encoded = crate::wire::encode(
            &request,
            self.effective_settings(
                &request,
                derived_cache_key.as_deref(),
                derived_safety_identifier.as_deref(),
            ),
        )?;
        // Where it goes is decided by the output, and a deployment that did not configure that
        // endpoint cannot serve it. Checked here, where the body is in hand, so the refusal names the
        // modality rather than whichever URL happened to be missing.
        self.endpoint_for(encoded.endpoint)?;
        // `active_fields` includes this adapter's own extensions; the encoder consumed exactly the
        // ones `effective_settings` read, so every own extension counts as honoured. Extensions for
        // other providers are filtered out by the kernel's contract check, not here.
        let mut honoured = request.active_fields();
        honoured.retain(|field| {
            !encoded
                .dropped
                .iter()
                .any(|dropped| dropped.field == *field)
        });
        let plan = nucleos::adapter::Plan {
            honoured,
            degraded: encoded.dropped.clone(),
        };
        Ok(nucleos::adapter::Prepared::new(plan, encoded))
    }

    fn count_tokens(&self, request: &ModelRequest) -> Result<u64> {
        // An estimate, and it says so: the provider does not publish a counter, and the kernel's
        // fit check only needs to be conservative. Debug level because it runs twice per turn.
        let tokens = request.text_len().div_ceil(4).max(1);
        tracing::debug!(
            event = "tokens_estimated",
            model = %self.config.model,
            tokens,
            characters = request.text_len(),
        );
        Ok(tokens)
    }

    async fn invoke(&self, prepared: nucleos::adapter::Prepared) -> Result<ModelStream> {
        // The body `prepare` compiled, not a second one: re-encoding here would reintroduce the
        // bug this split exists to remove.
        let Some(encoded) = prepared.take_artifact::<crate::wire::Encoded>() else {
            return Err(Error::internal(
                "the openai adapter was invoked with an artifact it did not compile",
            ));
        };

        let url = self.endpoint_for(encoded.endpoint)?;
        let mut surface = match encoded.endpoint {
            crate::wire::Endpoint::Responses => crate::decode::Surface::responses(),
            crate::wire::Endpoint::Images => crate::decode::Surface::images(
                encoded
                    .answer_media_type
                    .clone()
                    .unwrap_or_else(|| "image/png".to_owned()),
            ),
            // Handled above: speech is bytes, not frames, so it never reaches a decoder.
            crate::wire::Endpoint::Speech => crate::decode::Surface::responses(),
        };
        let mut http = self.http.post(url).json(&encoded.body);
        if let Some(key) = &self.config.api_key {
            http = http.bearer_auth(key);
        }
        let response = http.send().await.map_err(|error| {
            if error.is_timeout() {
                Error::timeout(format!("the model did not answer in time: {error}"))
            } else {
                Error::unavailable(format!("cannot reach {}: {error}", self.config.url))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(provider_error(status.as_u16(), &body));
        }

        // Speech answers with audio bytes, not frames: there is nothing to frame and no dialect to
        // read. Every chunk appends to the part at index 0 — the merge rule the contract defines for
        // media — so a long clip arrives as one part built from many chunks.
        if encoded.endpoint == crate::wire::Endpoint::Speech {
            let media_type = encoded
                .answer_media_type
                .unwrap_or_else(|| "audio/mpeg".to_owned());
            let chunks = response.bytes_stream();
            let stream = futures_util::stream::unfold(
                (chunks, media_type, false),
                |(mut chunks, media_type, done)| async move {
                    if done {
                        return None;
                    }
                    match chunks.next().await {
                        Some(Ok(bytes)) => {
                            let source =
                                match nucleos::contract::AudioSource::data(
                                    bytes.to_vec(),
                                    media_type.clone(),
                                ) {
                                    Ok(source) => source,
                                    Err(reason) => return Some((
                                        Err(Error::new(
                                            nucleos::contract::ErrorCode::InvalidProviderResponse,
                                            reason,
                                        )),
                                        (chunks, media_type, true),
                                    )),
                                };
                            Some((
                                Ok(ModelDelta::PartAppend {
                                    part_index: 0,
                                    part: nucleos::contract::Part::audio(source),
                                }),
                                (chunks, media_type, false),
                            ))
                        }
                        Some(Err(error)) => Some((
                            Err(Error::unavailable(format!(
                                "the audio stream broke: {error}"
                            ))),
                            (chunks, media_type, true),
                        )),
                        // The body ended: a clip has no finish reason to report, and the whole body
                        // arriving is the claim that it finished.
                        None => Some((
                            Ok(ModelDelta::Finished {
                                reason: FinishReason::Stop,
                            }),
                            (chunks, media_type, true),
                        )),
                    }
                },
            );
            return Ok(Box::pin(stream) as ModelStream);
        }

        let mut framer = crate::wire::Framer::default();

        let deltas = response.bytes_stream().flat_map(move |chunk| {
            let items: Vec<Result<ModelDelta>> = match chunk {
                Err(error) => vec![Err(Error::unavailable(format!(
                    "the stream broke: {error}"
                )))],
                Ok(bytes) => framer
                    .push(&bytes)
                    .into_iter()
                    .flat_map(|payload| match surface.push(&payload) {
                        Ok(deltas) => deltas.into_iter().map(Ok).collect::<Vec<_>>(),
                        Err(error) => vec![Err(error)],
                    })
                    .collect(),
            };
            tokio_stream::iter(items)
        });

        Ok(Box::pin(deltas) as ModelStream)
    }
}

/// A provider's refusal, in the kernel's taxonomy.
///
/// The status code decides the category and the body supplies the words: a 429 is a rate limit
/// whichever provider wrote it, and a caller that had to parse each provider's JSON to learn that
/// would be doing the kernel's job.
fn provider_error(status: u16, body: &str) -> Error {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(400).collect());

    let error = match status {
        401 | 403 => Error::denied(message),
        404 => Error::not_found(message),
        400 | 422 => Error::invalid(message),
        408 | 504 => Error::timeout(message),
        429 => Error::budget(message),
        500..=599 => Error::unavailable(message),
        _ => Error::internal(message),
    };
    // `param` and `code` are the actionable part of a refusal: "unsupported parameter
    // reasoning.effort" is a config fix, and a caller should not have to parse prose to find it.
    let error = error.detail("status", status.to_string());
    match parsed.as_ref().and_then(|value| value.get("error")) {
        Some(details) => {
            let error = match details.get("code").and_then(serde_json::Value::as_str) {
                Some(code) => error.detail("provider_code", code),
                None => error,
            };
            match details.get("param").and_then(serde_json::Value::as_str) {
                Some(param) => error.detail("param", param),
                None => error,
            }
        }
        None => error,
    }
}
