//! The kernel's error type.
//!
//! One struct, not an enum. Every field exists because the ABI's `Error` message has it, and
//! a kernel that translated its own error taxonomy into the wire's would be inventing a
//! second vocabulary for the same facts — the one that drifts.
//!
//! Categories live in [`ErrorCode`]: programs match on the code, humans read the reason.

use crate::contract::ErrorCode;
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;

/// The kernel's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong, in the shape the wire carries.
#[derive(Debug)]
pub struct Error {
    code: ErrorCode,
    reason: String,
    retry_after_millis: u64,
    details: BTreeMap<String, String>,
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    /// Build one from a code and a human-readable reason.
    #[must_use]
    pub fn new(code: ErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            retry_after_millis: 0,
            details: BTreeMap::new(),
            source: None,
        }
    }

    /// The request was malformed.
    #[must_use]
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, reason)
    }

    /// The thing named does not exist.
    #[must_use]
    pub fn not_found(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, reason)
    }

    /// Policy refused.
    #[must_use]
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Denied, reason)
    }

    /// A budget was exhausted.
    #[must_use]
    pub fn budget(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Budget, reason)
    }

    /// A deadline passed.
    #[must_use]
    pub fn timeout(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, reason)
    }

    /// A dependency was not reachable.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, reason)
    }

    /// A bug.
    #[must_use]
    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, reason)
    }

    /// When the caller may retry. Zero means "no hint", not "immediately".
    #[must_use]
    pub fn retry_after(mut self, millis: u64) -> Self {
        self.retry_after_millis = millis;
        self
    }

    /// Add a machine-readable detail the caller can act on, e.g. `needed_tokens`.
    #[must_use]
    pub fn detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    /// Attach the underlying cause. It is not sent on the wire — only the reason is.
    #[must_use]
    pub fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The category programs match on.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The human-readable reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// When the caller may retry, in milliseconds. Zero means unknown.
    #[must_use]
    pub fn retry_after_millis(&self) -> u64 {
        self.retry_after_millis
    }

    /// Machine-readable details.
    #[must_use]
    pub fn details(&self) -> &BTreeMap<String, String> {
        &self.details
    }

    /// Whether retrying the same request could succeed.
    ///
    /// Delegates to the code, because the answer belongs to the category: a caller that can
    /// see the code can derive this, and two sources of truth would eventually disagree.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.code.is_retryable()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.reason)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn StdError + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constructors_set_the_code_they_are_named_for() {
        assert_eq!(Error::invalid("x").code(), ErrorCode::Invalid);
        assert_eq!(Error::not_found("x").code(), ErrorCode::NotFound);
        assert_eq!(Error::denied("x").code(), ErrorCode::Denied);
        assert_eq!(Error::budget("x").code(), ErrorCode::Budget);
        assert_eq!(Error::timeout("x").code(), ErrorCode::Timeout);
        assert_eq!(Error::unavailable("x").code(), ErrorCode::Unavailable);
        assert_eq!(Error::internal("x").code(), ErrorCode::Internal);
    }

    #[test]
    fn a_new_error_has_no_retry_hint_and_no_details() {
        let error = Error::invalid("bad");
        assert_eq!(error.retry_after_millis(), 0);
        assert!(error.details().is_empty());
    }

    #[test]
    fn retry_after_sets_the_hint() {
        let error = Error::unavailable("down").retry_after(5000);
        assert_eq!(error.retry_after_millis(), 5000);
    }

    #[test]
    fn details_accumulate() {
        let error = Error::budget("out")
            .detail("needed_tokens", "1000")
            .detail("remaining", "0");
        assert_eq!(error.details().len(), 2);
        assert_eq!(
            error.details().get("needed_tokens"),
            Some(&"1000".to_owned())
        );
    }

    #[test]
    fn display_shows_the_code_and_the_reason() {
        let error = Error::not_found("no such session");
        let message = error.to_string();
        assert!(message.contains("NotFound"));
        assert!(message.contains("no such session"));
    }

    #[test]
    fn is_retryable_delegates_to_the_code() {
        // The exact set is ErrorCode's decision; this pins that Error does not have a second opinion.
        assert_eq!(
            Error::unavailable("x").is_retryable(),
            ErrorCode::Unavailable.is_retryable()
        );
        assert_eq!(
            Error::invalid("x").is_retryable(),
            ErrorCode::Invalid.is_retryable()
        );
    }

    #[test]
    fn a_source_is_attached_but_not_shown_in_display() {
        let cause = std::io::Error::other("disk full");
        let error = Error::internal("checkpoint failed").with_source(cause);
        // The source is reachable through the Error trait, but Display shows only the reason.
        assert!(StdError::source(&error).is_some());
        assert!(!error.to_string().contains("disk full"));
    }
}
