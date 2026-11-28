use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};
use crate::{Authority, Envelope};

/// An application's proposal to change its approved capabilities.
///
/// This changes nothing by itself. The application is authenticated by the transport and the
/// proposal must remain inside the deployment resource pool; only the user-facing management
/// operation can approve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestEnvelopeChange {
    /// The current envelope version this proposal was based on.
    pub base_version: u64,
    /// The complete replacement authority the application is asking for.
    pub authority: Authority,
    /// Why the application needs the change, for the user to read.
    pub reason: String,
}

impl RequestEnvelopeChange {
    /// Check that this proposal is current and within the deployment pool.
    ///
    /// It deliberately does not fit inside the current application envelope: expansion is the
    /// point of a proposal, and it does not become authority until user approval.
    pub fn validate(&self, current: &Envelope, deployment: &Authority) -> Result<()> {
        check_version(current, self.base_version)?;
        self.authority.fits_in(deployment)
    }
}

/// A user's approval of a complete replacement envelope.
///
/// The application is selected by the management operation's path, not by this body. On success
/// the runtime returns the new native [`crate::Envelope`] with `version + 1`. A stale base version
/// is a conflict, preventing two management clients from silently overwriting each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApproveEnvelope {
    /// The version the user reviewed.
    pub base_version: u64,
    /// The complete approved replacement.
    pub authority: Authority,
}

impl ApproveEnvelope {
    /// Produce the next envelope after the management plane has authenticated the user.
    pub fn apply(&self, current: &Envelope, deployment: &Authority) -> Result<Envelope> {
        let version = next_version(current, self.base_version)?;
        self.authority.fits_in(deployment)?;
        Ok(Envelope {
            app: current.app.clone(),
            version,
            authority: self.authority.clone(),
        })
    }
}

/// A user's removal of capabilities from an application.
///
/// `remaining` is the complete post-revocation authority rather than another per-resource mirror
/// of model, tool, sandbox and skill types. It must fit inside the current envelope. On success
/// the runtime returns the new native [`crate::Envelope`] with `version + 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevokeCapabilities {
    /// The current version the user reviewed.
    pub base_version: u64,
    /// The complete authority that remains after revocation.
    pub remaining: Authority,
}

impl RevokeCapabilities {
    /// Produce the next, strictly contained envelope after user authentication.
    pub fn apply(&self, current: &Envelope) -> Result<Envelope> {
        let version = next_version(current, self.base_version)?;
        self.remaining.fits_in(&current.authority)?;
        Ok(Envelope {
            app: current.app.clone(),
            version,
            authority: self.remaining.clone(),
        })
    }
}

fn check_version(current: &Envelope, base_version: u64) -> Result<()> {
    if current.version != base_version {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "the envelope changed: reviewed version {base_version}, current version {}",
                current.version
            ),
        )
        .at("base_version"));
    }
    Ok(())
}

fn next_version(current: &Envelope, base_version: u64) -> Result<u64> {
    check_version(current, base_version)?;
    current.version.checked_add(1).ok_or_else(|| {
        Error::new(
            ErrorKind::Conflict,
            "the envelope version cannot advance further",
        )
        .at("base_version")
    })
}

// GetEnvelope has no body and returns `crate::Envelope` directly. None of these version fields is
// consulted by a running session: enforcement always reads the current envelope, so revocation
// affects its next model, tool, sandbox or skill check immediately.
