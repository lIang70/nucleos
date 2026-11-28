use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Whether a tool reaches the model's definition list.
///
/// **Presentation only**: it never changes what a session may call. `Hidden` here
/// still answers an exact call by name — hiding is not forbidding, and a
/// deployment that wants both uses [`crate::ToolCeiling::hide`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    /// Defined every round. Keep this set small: it is the stable baseline, and
    /// the only way to subtract a search round-trip.
    Always,
    /// Appears when required by name or surfaced by discovery.
    Direct,
    /// Hidden until a search surfaces it. **The default**: a session that declares
    /// nothing sees one tool — the search itself.
    #[default]
    Deferred,
    /// Never appears. An exact call by name still works.
    Hidden,
}

impl Exposure {
    /// How visible this is, higher meaning more visible. Used to order eviction
    /// and to compare two sets for narrowing.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Always => 3,
            Self::Direct => 2,
            Self::Deferred => 1,
            Self::Hidden => 0,
        }
    }
}
