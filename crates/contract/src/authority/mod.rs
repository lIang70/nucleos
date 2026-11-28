//! The native capabilities available to a deployment, application, or session.
//!
//! This module is a composition of the contract's resource nouns, not a policy language:
//! inference uses [`ModelRef`] and [`Mode`], calls use [`ToolName`], execution uses [`Confine`],
//! and skill mounting uses [`SkillRootName`]. A resource outside the set is not allowed; there
//! is no intermediate target or rule representation to translate back at dispatch time.

mod capability;
mod envelope;
#[cfg(test)]
mod tests;

pub use capability::Authority;
pub use envelope::Envelope;
