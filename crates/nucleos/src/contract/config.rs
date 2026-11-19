//! What a session may show and how much of it: exposure, budgets, and the patch.

use super::{GenerateParams, Hints, Routing, SandboxPolicy};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How visible a tool is, as distinct from whether it may be used.
///
/// The ceiling (`config.tools`) is the application's to narrow and only ever narrows. Exposure is
/// what is in the prompt right now: it may grow up to the ceiling, by configuration or by a search
/// tool asking. Keeping the two apart is what makes dynamic tool loading safe.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    /// In the prompt every turn. Keep this set small: it is the stable baseline.
    Always,
    /// In the prompt once it has been found or used, and while it stays in the pool.
    Direct,
    /// Invisible until a search surfaces it. The default, and the reason discovery exists.
    #[default]
    Deferred,
    /// Never in the prompt, not even via search. Still callable by name — hiding a tool is not
    /// forbidding it.
    Hidden,
}

impl Exposure {
    /// Which of two visible tools to keep when the budget cannot hold both. Lower wins.
    ///
    /// `Always` outranks `Direct`, which outranks `Deferred`: a baseline that disappears because
    /// the model searched too much is worse than a found tool that expires.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Always => 0,
            Self::Direct => 1,
            Self::Deferred => 2,
            Self::Hidden => 3,
        }
    }

    /// Whether a search may surface it.
    #[must_use]
    pub fn searchable(self) -> bool {
        matches!(self, Self::Direct | Self::Deferred)
    }
}

/// How many tool definitions may reach the model per turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefinitionBudget {
    /// How many definitions, counted before the prompt is built.
    pub max_definitions: usize,
    /// What they may weigh together, in bytes of name, description and schema.
    pub max_bytes: u64,
}

impl Default for DefinitionBudget {
    /// Thirty-two definitions or 16 KiB, whichever comes first.
    ///
    /// Both caps, because the two failures are different: forty one-line tools are cheap and
    /// confusing, four tools with enormous schemas are expensive and clear.
    fn default() -> Self {
        Self {
            max_definitions: 32,
            max_bytes: 16 * 1024,
        }
    }
}

/// What a session remembers having found.
///
/// Separate from the per-turn budget on purpose: the budget caps what a prompt shows, the pool caps
/// what the session remembers. A session that can only show eight tools may still have discovered
/// thirty, and re-searching for one it used two turns ago is the waste this prevents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiscoveryPolicy {
    /// How many discovered tools stay remembered.
    pub max_tools: usize,
    /// What they may weigh together.
    pub max_bytes: u64,
    /// How many turns without a use before a tool is forgotten.
    pub idle_rounds: u64,
}

impl Default for DiscoveryPolicy {
    fn default() -> Self {
        Self {
            max_tools: 32,
            max_bytes: 16 * 1024,
            idle_rounds: 10,
        }
    }
}

/// The host's exposure policy for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolPolicy {
    /// The exposure of every tool without an entry in `exposures`. Defaults to `Deferred`.
    #[serde(default)]
    pub default: Exposure,
    /// Per-tool overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exposures: BTreeMap<String, Exposure>,
    /// What a prompt may carry this turn.
    #[serde(default)]
    pub budget: DefinitionBudget,
    /// What the session remembers.
    #[serde(default)]
    pub discovery: DiscoveryPolicy,
}

impl ToolPolicy {
    /// The exposure of one tool.
    #[must_use]
    pub fn exposure_of(&self, name: &str) -> Exposure {
        self.exposures.get(name).copied().unwrap_or(self.default)
    }

    /// Refuse a policy that would make discovery impossible.
    ///
    /// A hidden or deferred `tool_search` is a catalog nobody can reach: the model is told to search
    /// with a tool it cannot see. Absence is not a refusal — an unconfigured search tool is seeded
    /// to `Always` by the kernel, so the default policy discovers rather than hides.
    pub fn validate(&self, search_tool: &str) -> crate::error::Result<()> {
        if self
            .exposures
            .get(search_tool)
            .is_some_and(|exposure| *exposure != Exposure::Always)
        {
            return Err(crate::error::Error::invalid(format!(
                "{search_tool} must be exposed as always; any other exposure disables discovery"
            )));
        }
        Ok(())
    }
}

/// A partial update to a session's mutable configuration.
///
/// The wiring — kind, sandbox, workspace — is not here, because it is decided when the session
/// opens and is not the application's to change while it runs. Everything in this struct is
/// content or strategy, and every field is optional: absent means unchanged, `null` (where the
/// field is doubly optional) means cleared.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigPatch {
    /// Standing instructions. `Some(None)` clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Option<String>>,
    /// Generation parameters. Replaces the whole block, because every field is already optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<GenerateParams>,
    /// Adapter hints. Merged field by field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<Hints>,
    /// The sandbox, when the change only narrows it.
    ///
    /// Widening is refused here on purpose: more room for the whole session is an escalation, and an
    /// escalation is a gated decision about one command, not a field in a configuration call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxPolicy>,
    /// The tool allow-list. Empty means every registered tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The exposure policy. Absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<ToolPolicy>,
    /// The route this session takes. Absent means unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
}
