//! Named skill resources shared by mounting and authority.
//!
//! A root is a deployment-declared name, never a caller-supplied filesystem path: it is the unit
//! authority speaks about. Individual skills discovered inside a mounted root are [`Skill`] entries —
//! what the catalogue lists, and what the file tree reads.

mod item;
mod root;
#[cfg(test)]
mod tests;

pub use item::Skill;
pub use root::SkillRootName;
