use super::*;
use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The argument object of one call.
///
/// The bytes are private and every constructor copies them. A call that has
/// entered the log is immutable — it is content-addressed — and a caller still
/// holding the source `Vec<u8>` would otherwise be able to change a
/// conversation that has already been recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arguments(Box<str>);

impl Default for Arguments {
    fn default() -> Self {
        Self::empty()
    }
}

impl Arguments {
    /// Wrap an encoded argument object. The bytes are copied.
    #[must_use]
    pub fn new(raw: impl Into<Box<str>>) -> Self {
        Self(raw.into())
    }

    /// An empty object.
    #[must_use]
    pub fn empty() -> Self {
        Self("{}".into())
    }

    /// The encoded object.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// Decode into a caller's type.
    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        serde_json::from_str(&self.0)
            .map_err(|error| Error::invalid(format!("tool arguments do not decode: {error}")))
    }
}

impl Serialize for Arguments {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> StdResult<S::Ok, S::Error> {
        let value: serde_json::Value =
            serde_json::from_str(&self.0).map_err(serde::ser::Error::custom)?;
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Arguments {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> StdResult<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let encoded = serde_json::to_string(&value).map_err(serde::de::Error::custom)?;
        Ok(Self(encoded.into()))
    }
}

impl JsonSchema for Arguments {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ToolArguments".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <serde_json::Value as JsonSchema>::json_schema(generator)
    }
}

type StdResult<T, E> = std::result::Result<T, E>;

/// One call the model asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    /// The provider-assigned id. Results echo it, and a batch folds back in this
    /// order.
    pub id: String,
    /// Which tool.
    pub name: ToolName,
    /// What to pass. Opaque here: decoding it is the tool's business.
    #[serde(default)]
    pub arguments: Arguments,
}

/// What one call produced.
///
/// Three fields, and no more. A tool does not report cost here and does not ask
/// for visibility here: the first is accounting's vocabulary and the second is a
/// projection's, and both would make a result carry an opinion it cannot be
/// held to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// Which call this answers.
    pub call_id: String,
    /// What to hand the model. Content, not a string: a tool that returns an
    /// image and one that returns prose travel the same path.
    pub content: Content,
    /// Whether the model should read this as a failure.
    ///
    /// The error text is the recovery information, so a failure is content with
    /// this flag rather than an absent result.
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    /// A successful result.
    #[must_use]
    pub fn ok(call_id: impl Into<String>, content: Content) -> Self {
        Self {
            call_id: call_id.into(),
            content,
            is_error: false,
        }
    }

    /// A failure the model must read.
    #[must_use]
    pub fn failed(call_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: Content::text(message),
            is_error: true,
        }
    }
}
