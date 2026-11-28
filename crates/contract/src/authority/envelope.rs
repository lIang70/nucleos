use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::AppId;

use super::Authority;

/// The authority currently approved for one application.
///
/// Sessions retain no cached copy for enforcement: each action reads the application's current
/// envelope, so removing a capability restricts running sessions on their next action. The
/// version is an audit and storage coordinate, not a permission-expiry check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    /// Which application this approval belongs to.
    pub app: AppId,
    /// Monotonically increasing approval version.
    pub version: u64,
    /// The concrete native capabilities the application may use.
    pub authority: Authority,
}

impl Envelope {
    /// Check that a session plan fits inside this current approval.
    pub fn validate(&self, session: &Authority) -> Result<()> {
        session.fits_in(&self.authority)
    }
}
