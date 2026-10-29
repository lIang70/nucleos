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
