//! An OpenAI-compatible model, as a [`Model`] adapter — over the **Responses API**.
//!
//! One implementation covers the whole family that speaks `/v1/responses`: OpenAI itself, and the
//! compatible endpoints that followed it. What they disagree about is capability, so capability is
//! configuration rather than a guess: an adapter that assumed tool support and found none would
//! fail somewhere much less obvious than its own constructor.
//!
//! The crate exists because the kernel must not depend on an HTTP client. `nucleos` owns the
//! mechanism; this owns one provider's dialect.

pub mod decode;
pub mod wire;

use async_trait::async_trait;
use decode::Decoder;
use futures_util::StreamExt;
use nucleos::adapter::{Model, ModelDelta, ModelRequest, ModelStream};
use nucleos::contract::Capability;
use nucleos::error::{Error, Result};
use std::time::Duration;

/// How to reach one model.
#[derive(Debug, Clone)]
pub struct Config {
    /// The full URL of the responses endpoint.
    ///
    /// Full rather than a base: compatible endpoints disagree about where the path goes, and
    /// guessing for them is how a working config breaks on the next one.
    pub url: String,
    /// A bearer token, when the endpoint wants one. `None` is a local server.
    pub api_key: Option<String>,
    /// The model name to send.
    pub model: String,
    /// What this model can do. Configuration, not discovery.
    pub capability: Capability,
    /// This deployment's default hints. A call's hints override them field by field.
    pub options: nucleos::contract::Hints,
    /// The provider's prefix-cache key.
    ///
    /// The host derives it, usually from the session id, so that a session's turns hit the same
    /// cache. It is not the application's to choose: a wrong key costs money and changes nothing
    /// the application can observe.
    pub prompt_cache_key: Option<String>,
    /// Extra data to ask the provider for.
    pub include: Vec<wire::Include>,
    /// A privacy-preserving identifier for the end user, derived from the principal.
    pub safety_identifier: Option<String>,
    /// Whether tool schemas are declared strict.
    ///
    /// Only for schemas that are eligible: `additionalProperties: false`, and every property
    /// listed as required. The provider rejects `strict: true` otherwise, and it rejects the whole
    /// call — so the default is off, and turning it on is a deliberate statement that whoever
    /// produced the schemas did the work.
    pub strict_tools: bool,
    /// Whether it accepts `reasoning.effort`.
    ///
    /// Asked of the caller rather than probed, because the alternative is a 400 that kills a call
    /// over a hint. When false, a requested effort is reported as dropped and the call proceeds.
    pub supports_reasoning: bool,
    /// How long to wait for the response headers.
    pub timeout: Option<Duration>,
}

impl Config {
    /// A capability set that assumes nothing surprising.
    #[must_use]
    pub fn capabilities(context_window: u32, max_output_tokens: u32) -> Capability {
        Capability {
            tools: true,
            tool_choice_required: true,
            structured_output: true,
            streaming: true,
            context_window,
            max_output_tokens,
        }
    }
}

/// One model behind the kernel's [`Model`] contract.
#[derive(Clone)]
pub struct OpenAi {
    config: Config,
    http: reqwest::Client,
}

impl std::fmt::Debug for OpenAi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is a secret; the model and the URL are what a reader wants.
        f.debug_struct("OpenAi")
            .field("url", &self.config.url)
            .field("model", &self.config.model)
            .field("supports_reasoning", &self.config.supports_reasoning)
            .finish_non_exhaustive()
    }
}

impl OpenAi {
    /// Build an adapter.
    pub fn new(config: Config) -> Result<Self> {
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
        Ok(Self { config, http })
    }
}

#[async_trait]
impl Model for OpenAi {
    fn capability(&self) -> Capability {
        self.config.capability
    }

    /// A conservative estimate, and it says so by being one.
    ///
    /// This adapter carries no tokenizer. Four characters per token is the usual rule of thumb and
    /// it under-estimates for code and for CJK — which is the safe direction: the kernel uses it to
    /// refuse a turn that will not fit, and refusing early is recoverable while overflowing is not.
    fn count_tokens(&self, request: &ModelRequest) -> Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }

    async fn invoke(&self, request: ModelRequest) -> Result<ModelStream> {
        let encoded = wire::encode(
            &request,
            wire::Settings {
                model: &self.config.model,
                supports_reasoning: self.config.supports_reasoning,
                strict_tools: self.config.strict_tools,
                options: &self.config.options,
                cache_key: self.config.prompt_cache_key.as_deref(),
                include: &self.config.include,
                safety_identifier: self.config.safety_identifier.as_deref(),
            },
        );

        let mut http = self.http.post(&self.config.url).json(&encoded.body);
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

        // A degradation is reported before anything else, so the record shows why the request is
        // not quite what the caller asked for even when the answer looks fine.
        let reported: Vec<Result<ModelDelta>> = encoded
            .dropped
            .into_iter()
            .map(|dropped| {
                Ok(ModelDelta::Dropped {
                    field: "request",
                    reason: format!("{}: {}", dropped.field, dropped.reason),
                })
            })
            .collect();

        let mut framer = wire::Framer::default();
        let mut decoder = Decoder::default();

        let deltas = response.bytes_stream().flat_map(move |chunk| {
            let items: Vec<Result<ModelDelta>> = match chunk {
                Err(error) => vec![Err(Error::unavailable(format!(
                    "the stream broke: {error}"
                )))],
                Ok(bytes) => framer
                    .push(&bytes)
                    .into_iter()
                    .flat_map(|payload| match decoder.push(&payload) {
                        Ok(deltas) => deltas.into_iter().map(Ok).collect::<Vec<_>>(),
                        Err(error) => vec![Err(error)],
                    })
                    .collect(),
            };
            tokio_stream::iter(items)
        });

        Ok(Box::pin(tokio_stream::iter(reported).chain(deltas)) as ModelStream)
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
