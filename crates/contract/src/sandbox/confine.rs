use super::*;
use crate::error::{Error, ErrorKind, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a backend instance is built to enforce.
///
/// Fixed when the instance is created, because a backend serves exactly one
/// session — so "the policy of this call" and "the policy of this session" are the
/// same thing, and a per-call field would re-open what it means to run a narrower
/// call inside a wider boundary.
///
/// Resources are the one exception: [`Confine::harden`] lets a call ask for less.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Confine {
    /// The file boundary.
    #[serde(default)]
    pub files: Files,
    /// The egress posture.
    #[serde(default)]
    pub net: NetPolicy,
    /// Which environment the child sees.
    #[serde(default)]
    pub env: EnvPolicy,
    /// How a command may be run.
    #[serde(default)]
    pub exec: ExecPolicy,
    /// The ceiling for one command.
    #[serde(default)]
    pub resources: Resources,
}

impl Confine {
    /// Validate every dimension at once.
    pub fn validate(&self) -> Result<()> {
        self.files.normalized().map(|_| ())
    }

    /// Whether `next` confines at least as tightly as `self`.
    ///
    /// The only direction a configuration change may move.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        self.files.narrows_to(&next.files)
            && self.net.narrows_to(&next.net)
            && self.env.narrows_to(&next.env)
            && self.exec.narrows_to(&next.exec)
            && next.resources.narrowed_to(&self.resources) == next.resources
    }

    /// Intersect two sandbox boundaries.
    ///
    /// `narrows_to` establishes a partial order, not a total one. Two boundaries with unrelated
    /// writable roots have no single safe representative here, so that case is a conflict rather
    /// than a silent choice of either side.
    pub fn meet(&self, other: &Self) -> Result<Self> {
        if self.narrows_to(other) {
            return Ok(other.clone());
        }
        if other.narrows_to(self) {
            return Ok(self.clone());
        }
        Err(Error::new(
            ErrorKind::Conflict,
            "sandbox confinements have no safe intersection",
        ))
    }

    /// The effective resources for one call — the only place a per-call budget is
    /// resolved, and it resolves in one direction only.
    #[must_use]
    pub fn harden(&self, call: &Resources) -> Self {
        Self {
            resources: self.resources.narrowed_to(call),
            ..self.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// What a backend can enforce
// ---------------------------------------------------------------------------
