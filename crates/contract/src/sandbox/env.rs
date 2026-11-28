use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which host environment the child may see.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct EnvPolicy {
    /// Variables passed through. Empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Values the policy injects.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inject: BTreeMap<String, String>,
}

impl EnvPolicy {
    /// Whether `next` exposes no more than `self` does.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        next.allow.iter().all(|name| self.allow.contains(name))
            && next
                .inject
                .iter()
                .all(|(key, value)| self.inject.get(key) == Some(value))
    }
}
