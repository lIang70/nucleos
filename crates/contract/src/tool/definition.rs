use crate::content::ToolName;
use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a model is told a tool accepts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolDefinition {
    /// The name the model calls.
    pub name: ToolName,
    /// One or two sentences: what it does, when to use it.
    pub description: String,
    /// A JSON Schema **object**. Kept as raw JSON so a request never exposes an
    /// untyped provider-parameter map.
    #[serde(default = "empty_object")]
    pub input_schema: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl ToolDefinition {
    /// Check the shape a provider will rely on.
    pub fn validate(&self) -> Result<()> {
        if !self.input_schema.is_object() {
            return Err(Error::invalid(format!(
                "tool {:?} input schema must be a JSON object",
                self.name
            )));
        }
        Ok(())
    }
}
