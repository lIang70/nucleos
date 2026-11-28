use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Outcome;
use super::{ExtensionSet, ModelRef};
use crate::content::Content;
use crate::error::{Error, Result};

/// What shape one answer takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Yes or no; the answer carries P(true).
    Predicate,
    /// Pick one of the declared choices.
    Choice,
    /// A value on a declared scale.
    Score,
}

/// One question in a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    /// Identifies the question; callers use this to correlate their input with the answer order.
    pub name: String,
    /// What shape the answer takes.
    pub kind: QuestionKind,
    /// The question text.
    pub text: String,
    /// The choices for [`QuestionKind::Choice`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// A probability held as parts per million.
///
/// Not an `f32`: probabilities are compared, stored and carried across languages. `1_000_000`
/// is certainty and `0` is impossibility.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "u32")]
pub struct Probability(u32);

impl Probability {
    /// Certainty.
    pub const CERTAIN: Self = Self(1_000_000);
    /// Impossibility.
    pub const NEVER: Self = Self(0);

    /// From parts per million, or `None` above certainty.
    #[must_use]
    pub fn from_parts(parts: u32) -> Option<Self> {
        (parts <= 1_000_000).then_some(Self(parts))
    }

    /// The parts per million.
    #[must_use]
    pub fn parts(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for Probability {
    type Error = Error;

    fn try_from(parts: u32) -> Result<Self> {
        Self::from_parts(parts)
            .ok_or_else(|| Error::invalid("a probability must be between 0 and 1,000,000 parts"))
    }
}

/// One typed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// P(true).
    Predicate {
        /// How likely "yes" is.
        p_true: Probability,
    },
    /// The chosen label.
    Choice {
        /// One of the question's declared choices.
        choice: String,
    },
    /// A value on the declared scale, normalized to 0..=1.
    Score {
        /// Where on the scale.
        value: Probability,
    },
    /// The model declined to answer this question.
    Refused {
        /// Why.
        reason: String,
    },
}

/// One decision batch.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DecideRequest {
    /// Which model decides.
    pub model: Option<ModelRef>,
    /// What the questions are about.
    pub input: Content,
    /// The questions, answered in order.
    pub questions: Vec<Question>,
    /// Provider-specific options.
    pub extensions: ExtensionSet,
}

/// The answers in question order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DecisionResponse {
    /// Which model answered.
    pub model: ModelRef,
    /// One answer per question.
    pub answers: Vec<Decision>,
    /// Which model answered, and what the batch spent.
    pub outcome: Outcome,
}

impl DecisionResponse {
    /// Check the accounting, and that every question got exactly one answer.
    ///
    /// # Errors
    ///
    /// A broken breakdown, or a response that answers a different number of questions than it was
    /// asked.
    pub fn validate(&self, expected: usize) -> crate::error::Result<()> {
        self.outcome.validate()?;
        if self.answers.len() != expected {
            return Err(crate::error::Error::internal(format!(
                "{expected} questions were answered {} times",
                self.answers.len()
            ))
            .at("answers"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn probability_is_a_bounded_integer_on_the_wire() {
        assert_eq!(Probability::from_parts(1_000_001), None);
        assert_eq!(
            serde_json::to_string(&Probability::CERTAIN).unwrap(),
            "1000000"
        );
        assert!(serde_json::from_str::<Probability>("1000001").is_err());
    }

    #[test]
    fn typed_answers_round_trip_without_losing_refusals() {
        let response = DecisionResponse {
            model: ModelRef::new("openai/judge-1").unwrap(),
            answers: vec![
                Decision::Predicate {
                    p_true: Probability::from_parts(750_000).unwrap(),
                },
                Decision::Refused {
                    reason: "not enough evidence".to_owned(),
                },
            ],
            outcome: Outcome::default(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: DecisionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, response);
    }
}
