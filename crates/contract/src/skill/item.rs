use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One skill the session may offer.
///
/// Three fields, and no body. The description is one line because it is rendered into a prompt, and
/// the body is *not* here because reading it is the file tree's job: a `load` of its own would be a
/// second way to read bytes, outside the session's file confinement and outside its failure
/// reporting.
///
/// `path` is absolute so the reader does not depend on a working directory, and it must resolve
/// inside a mounted skill root — which is what makes the file boundary, not this catalogue, the
/// thing that decides what is reachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Skill {
    /// What the model names it by.
    pub name: String,
    /// One line: what it is for, when to reach for it.
    pub description: String,
    /// Where its body lives.
    pub path: String,
}
