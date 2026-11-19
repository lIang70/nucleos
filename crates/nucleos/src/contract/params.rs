//! What a caller may ask a model for on one call, and what an adapter says it honoured.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One knob a request can set.
///
/// A vocabulary rather than a free string: [`Degraded`] names the knob it could not honour, and two
/// spellings of one knob turn "how often is `stop` dropped" into a question nobody can answer. It is
/// also what makes an adapter's silence detectable — a request reports the fields it set, an adapter
/// reports the fields it will honour, and the kernel refuses to call one that did not account for the
/// difference.
///
/// The set is small on purpose: one name per knob that appears on [`crate::adapter::ModelRequest`],
/// not one name per provider parameter. A knob only one provider understands is an
/// [`Field::Extension`], qualified with the provider and extension that own it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub enum Field {
    /// The standing instructions.
    System,
    /// The conversation.
    Messages,
    /// The tool declarations.
    Tools,
    /// How to choose among them.
    ToolChoice,
    /// What the call asks the model to produce.
    Output,
    /// One sampling knob from [`GenerateParams`].
    Param(Param),
    /// One adapter hint from [`Hints`].
    Hint(Hint),
    /// A provider-specific option, named by the extension that owns it.
    Extension {
        /// The provider the option belongs to.
        provider: String,
        /// The extension within that provider.
        id: String,
        /// The option within that extension.
        option: String,
    },
}

/// A sampling knob, named the way it is spelled on the wire: `param.temperature`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Param {
    /// The completion ceiling.
    MaxOutputTokens,
    /// Sampling temperature.
    Temperature,
    /// Nucleus sampling.
    TopP,
    /// Stop sequences.
    Stop,
    /// Reasoning effort.
    ReasoningEffort,
    /// Whether one turn may call several tools.
    ParallelToolCalls,
    /// The shape the answer must have.
    Format,
}

/// An adapter hint, named the way it is spelled on the wire: `hint.verbosity`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Hint {
    /// How much prose an answer should carry.
    Verbosity,
    /// How much reasoning to summarise.
    ReasoningSummary,
    /// The cost/latency trade.
    ServiceTier,
    /// A ceiling on tool calls in one response.
    MaxToolCalls,
    /// What to do when the conversation does not fit.
    Truncation,
    /// Labels the provider echoes back.
    Metadata,
}

impl Field {
    /// The wire spelling: `tools`, `param.stop`, `extension.openai.generation.effort`.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::System => "system".to_owned(),
            Self::Messages => "messages".to_owned(),
            Self::Tools => "tools".to_owned(),
            Self::ToolChoice => "tool_choice".to_owned(),
            Self::Output => "output".to_owned(),
            Self::Param(param) => format!("param.{}", param.name()),
            Self::Hint(hint) => format!("hint.{}", hint.name()),
            Self::Extension {
                provider,
                id,
                option,
            } => format!("extension.{provider}.{id}.{option}"),
        }
    }
}

impl Param {
    /// The name as it appears in a qualified field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::MaxOutputTokens => "max_output_tokens",
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::Stop => "stop",
            Self::ReasoningEffort => "reasoning_effort",
            Self::ParallelToolCalls => "parallel_tool_calls",
            Self::Format => "format",
        }
    }
}

impl Hint {
    /// The name as it appears in a qualified field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Verbosity => "verbosity",
            Self::ReasoningSummary => "reasoning_summary",
            Self::ServiceTier => "service_tier",
            Self::MaxToolCalls => "max_tool_calls",
            Self::Truncation => "truncation",
            Self::Metadata => "metadata",
        }
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl TryFrom<String> for Field {
    type Error = String;

    fn try_from(text: String) -> std::result::Result<Self, Self::Error> {
        let fail = || format!("unknown field {text:?}");
        if let Some(rest) = text.strip_prefix("param.") {
            return match rest {
                "max_output_tokens" => Ok(Self::Param(Param::MaxOutputTokens)),
                "temperature" => Ok(Self::Param(Param::Temperature)),
                "top_p" => Ok(Self::Param(Param::TopP)),
                "stop" => Ok(Self::Param(Param::Stop)),
                "reasoning_effort" => Ok(Self::Param(Param::ReasoningEffort)),
                "parallel_tool_calls" => Ok(Self::Param(Param::ParallelToolCalls)),
                "format" => Ok(Self::Param(Param::Format)),
                _ => Err(fail()),
            };
        }
        if let Some(rest) = text.strip_prefix("hint.") {
            return match rest {
                "verbosity" => Ok(Self::Hint(Hint::Verbosity)),
                "reasoning_summary" => Ok(Self::Hint(Hint::ReasoningSummary)),
                "service_tier" => Ok(Self::Hint(Hint::ServiceTier)),
                "max_tool_calls" => Ok(Self::Hint(Hint::MaxToolCalls)),
                "truncation" => Ok(Self::Hint(Hint::Truncation)),
                "metadata" => Ok(Self::Hint(Hint::Metadata)),
                _ => Err(fail()),
            };
        }
        if let Some(rest) = text.strip_prefix("extension.") {
            let mut parts = rest.splitn(3, '.');
            let (Some(provider), Some(id), Some(option)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(fail());
            };
            return Ok(Self::Extension {
                provider: provider.to_owned(),
                id: id.to_owned(),
                option: option.to_owned(),
            });
        }
        match text.as_str() {
            "system" => Ok(Self::System),
            "messages" => Ok(Self::Messages),
            "tools" => Ok(Self::Tools),
            "tool_choice" => Ok(Self::ToolChoice),
            "output" => Ok(Self::Output),
            _ => Err(fail()),
        }
    }
}

impl From<Field> for String {
    fn from(field: Field) -> Self {
        field.as_str()
    }
}

/// A field the provider could not honour.
///
/// Recorded rather than swallowed: a `tool_choice = Required` that quietly became `Auto` is a
/// session behaving differently than it was configured to, and the difference is
/// undiscoverable afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Degraded {
    /// The request field, e.g. [`Field::ToolChoice`].
    pub field: Field,
    /// Why, in the provider's terms.
    pub reason: String,
    /// A stable, driver-defined machine label for the adjustment — `implicit_off`, `quantized`,
    /// `not_supported`.
    ///
    /// A caller aggregates on the code rather than parsing the sentence, which is how a counter can
    /// answer "how often is a knob dropped here" without learning the provider's phrasing. Absent is
    /// allowed: a provider that cannot label it still says what happened, but cannot be counted on
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Degraded {
    /// A degradation without an explanation is a second silent failure.
    ///
    /// # Errors
    ///
    /// An empty or whitespace-only reason.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.reason.trim().is_empty() {
            return Err(format!(
                "a degradation of `{}` has no explanation",
                self.field.as_str()
            ));
        }
        Ok(())
    }
}

/// How the model should choose among the tools it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// It decides.
    #[default]
    Auto,
    /// It must call one.
    Required,
    /// It must not call one.
    None,
    /// It must call this one, by name.
    ///
    /// The name is the whole point: structured extraction asks for one function and reads its
    /// arguments, and "call something" is not the same instruction. The kernel refuses a name the
    /// request does not offer, because a forced call to a tool the model cannot see fails at the
    /// provider and costs a round trip to find out.
    Function {
        /// The tool it must call.
        name: String,
    },
}

/// How hard a reasoning model should think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Barely.
    Minimal,
    /// A little.
    Low,
    /// Normal.
    Medium,
    /// As much as it costs.
    High,
}

/// The shape the answer must have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextFormat {
    /// Prose.
    #[default]
    Text,
    /// A JSON object, schema unchecked.
    JsonObject,
    /// A JSON object that must match a schema.
    JsonSchema {
        /// The name the provider files it under.
        name: String,
        /// The schema, serialized.
        schema_json: String,
    },
}

/// What the caller wants from one call.
///
/// Every field is carried, not decided: the kernel has no opinion about sampling. They live
/// here rather than as a bag of extras because a bag is a second extension hatch, and these are
/// the parameters every provider has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GenerateParams {
    /// The completion ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Sequences that end the completion.
    ///
    /// Not every provider has this — the OpenAI Responses API rejects it outright — so an adapter
    /// that cannot send it reports a [`Degraded`] rather than dropping it in silence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Reasoning effort, for models that expose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// How to choose among tools.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Whether one turn may call several tools at once.
    ///
    /// In the contract rather than in an adapter because the *kernel* acts on it: `invoke_tools`
    /// fans a batch out concurrently. A provider that forbids it and a kernel that does it anyway
    /// would disagree about what happened in a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The shape the answer must have.
    #[serde(default)]
    pub format: TextFormat,
}

impl GenerateParams {
    /// Field-by-field merge: `over` wins where it sets something, and the rest is inherited.
    ///
    /// A per-call override is a patch, not a replacement: a caller changing only the temperature
    /// should not also have to repeat the stop sequences, the format and the tool choice. Defaults
    /// mean "inherit", which is why `Auto` and `Text` are not treated as overrides.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        Self {
            max_output_tokens: over.max_output_tokens.or(self.max_output_tokens),
            temperature: over.temperature.or(self.temperature),
            top_p: over.top_p.or(self.top_p),
            stop: if over.stop.is_empty() {
                self.stop.clone()
            } else {
                over.stop.clone()
            },
            reasoning_effort: over.reasoning_effort.or(self.reasoning_effort),
            tool_choice: if over.tool_choice == ToolChoice::Auto {
                self.tool_choice.clone()
            } else {
                over.tool_choice.clone()
            },
            parallel_tool_calls: over.parallel_tool_calls.or(self.parallel_tool_calls),
            format: if over.format == TextFormat::Text {
                self.format.clone()
            } else {
                over.format.clone()
            },
        }
    }
}
