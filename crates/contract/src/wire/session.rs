use crate::{Authority, SessionId, SessionKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Ask the runtime to open a caller-named session.
///
/// The application comes from authentication. The runtime checks `authority` against the current
/// application [`crate::Envelope`] before creating anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenSession {
    /// The caller-chosen name, unique within the authenticated application.
    pub session: SessionId,
    /// The native capabilities this session wants to use.
    pub authority: Authority,
}

/// The current public view of a session.
///
/// Returned by open, get and configure. `envelope_version` records the application approval the
/// runtime observed for this view; it is not a token that can keep a revoked capability alive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    /// The complete application-scoped identity.
    pub key: SessionKey,
    /// The session's effective plan, already checked against the application envelope.
    pub authority: Authority,
    /// The envelope version observed when this view was produced, for audit only.
    pub envelope_version: u64,
}

/// Replace a session's plan within the current application envelope.
///
/// The session name comes from the operation path and the application from authentication, so
/// neither is repeated in the body. This is not a narrow-only operation: a session may ask for a
/// different subset, but every request is checked against the current envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigureSession {
    /// The replacement native capability set.
    pub authority: Authority,
}

// Get and Close deliberately have no body types. Get returns `SessionView`; Close has no content
// on success. Inference has no session wrapper either: its path selects the session and its body
// is exactly `crate::ModelRequest`.
