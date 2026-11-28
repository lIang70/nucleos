//! One error shape, with a coordinate.
//!
//! `kind` is what a program branches on, `field` is which part of the request to
//! fix, and `message` is for a human. Three parts, because a single string
//! forces every caller to parse prose.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The kernel's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// What kind of failure this is.
///
/// Closed on purpose: dashboards group by it, so adding a variant is a
/// vocabulary change rather than a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The request's shape is wrong. Never retryable.
    Invalid,
    /// Policy refused it.
    Denied,
    /// The session or resource does not exist — or is not this caller's, which
    /// answers the same way so an id cannot be used to probe.
    NotFound,
    /// The name is taken, or the state contradicts the request.
    Conflict,
    /// Someone else holds the session; the caller should wait.
    Locked,
    /// The budget is spent.
    Budget,
    /// Nothing serves that model.
    UnknownModel,
    /// The provider is temporarily unavailable.
    Unavailable,
    /// The call exceeded its deadline.
    Timeout,
    /// The provider's stream stopped early. It may already have cost money.
    Truncated,
    /// The payload could not be decoded. Not retryable: a retry hands the same
    /// bytes to the same decoder.
    Unusable,
    /// The provider does not serve the requested output or feature.
    Unsupported,
    /// The caller cancelled.
    Cancelled,
    /// An effect started and nobody recorded how it ended.
    ///
    /// A resume that guesses repeats a side effect or skips one; refusing is the
    /// only answer that does not lie. The caller acknowledges the uncertainty
    /// explicitly to continue.
    EffectUncertain,
    /// A bug in the kernel.
    Internal,
}

impl ErrorKind {
    /// Whether resending the same request could succeed.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::Timeout | Self::Truncated | Self::Locked
        )
    }
}

/// One failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Error {
    /// What kind.
    pub kind: ErrorKind,
    /// Which part of the request to fix, when one is implicated.
    ///
    /// A **dotted path**, not a typed field: `"messages[3].content"`,
    /// `"extensions.openai/generation"`. This module is the bottom of the crate, so it
    /// cannot name a seam — and a path is what a caller needs in order to find the spot
    /// anyway. A seam that reports *degradation* uses its own typed vocabulary for it
    /// (`inference::Warning`), because a warning is addressed to the caller of that seam,
    /// not to everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// For a human.
    pub message: String,
    /// How long to wait before retrying, when the failure names a delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_millis: Option<u64>,
}

impl Error {
    /// Build one.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            field: None,
            message: message.into(),
            retry_after_millis: None,
        }
    }

    /// Point it at a part of the request.
    #[must_use]
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.field = Some(path.into());
        self
    }

    /// The request's shape is wrong.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }

    /// Policy refused.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Denied, message)
    }

    /// Not found, or not yours.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// A bug.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// Whether resending could succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transient_kinds_are_the_retryable_ones() {
        for kind in [
            ErrorKind::Unavailable,
            ErrorKind::Timeout,
            ErrorKind::Truncated,
        ] {
            assert!(kind.is_retryable(), "{kind:?}");
        }
    }

    #[test]
    fn a_permanent_failure_is_not_retryable() {
        for kind in [
            ErrorKind::Invalid,
            ErrorKind::Denied,
            ErrorKind::NotFound,
            ErrorKind::Conflict,
            ErrorKind::EffectUncertain,
            ErrorKind::Budget,
            ErrorKind::UnknownModel,
            ErrorKind::Unusable,
            ErrorKind::Unsupported,
            ErrorKind::Internal,
        ] {
            assert!(!kind.is_retryable(), "{kind:?}");
        }
    }

    #[test]
    fn unusable_is_deliberately_not_retryable() {
        assert!(
            !ErrorKind::Unusable.is_retryable(),
            "the payload is already unreadable; a retry hands the same bytes to the same decoder"
        );
    }
}
