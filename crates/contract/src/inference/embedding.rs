use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Outcome;
use super::{ExtensionSet, ModelRef};
use crate::content::Content;

/// One embedding call.
///
/// A document is content, not an utterance: it has no role and may contain several parts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct EmbedRequest {
    /// Which model embeds.
    pub model: Option<ModelRef>,
    /// One element per document.
    pub input: Vec<Content>,
    /// A requested width. `None` means the model's declared default.
    pub dimensions: Option<u32>,
    /// Provider-specific options.
    pub extensions: ExtensionSet,
}

/// Vectors in input order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EmbedResponse {
    /// Which model produced the vectors.
    ///
    /// An embedding model is not the chat model, and a vector whose producer cannot be named cannot
    /// be compared with another one.
    pub model: ModelRef,
    /// One vector per input document.
    pub vectors: Vec<Vec<f32>>,
    /// Which model answered, and what the batch spent.
    pub outcome: Outcome,
}

impl EmbedResponse {
    /// Check the accounting, and that every input got exactly one vector.
    ///
    /// # Errors
    ///
    /// A broken breakdown, or a response that does not answer each document once.
    pub fn validate(&self, expected: usize) -> crate::error::Result<()> {
        self.outcome.validate()?;
        if self.vectors.len() != expected {
            return Err(crate::error::Error::internal(format!(
                "{} documents were embedded into {} vectors",
                expected,
                self.vectors.len()
            ))
            .at("vectors"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn an_embedding_batch_round_trips_in_input_order() {
        let response = EmbedResponse {
            model: ModelRef::new("openai/embed-1").unwrap(),
            vectors: vec![vec![0.0, 0.5], vec![1.0, -0.5]],
            outcome: Outcome::default(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: EmbedResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, response);
    }
}
