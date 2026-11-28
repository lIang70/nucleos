use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::content::ToolName;
use crate::error::{Error, Result};
use crate::inference::{Mode, ModelRef};
use crate::sandbox::Confine;
use crate::skill::SkillRootName;

/// A set of native capabilities.
///
/// The same shape describes a deployment's resource pool, an application's approved envelope,
/// and a session's plan. Empty sets contain no resources. There is deliberately no wildcard:
/// configuration code may expand a pattern against the deployment catalogue, but the ABI holds
/// the concrete result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Authority {
    /// Each model and the operations it may perform.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<ModelRef, BTreeSet<Mode>>,
    /// Tools that may be exposed and called.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tools: BTreeSet<ToolName>,
    /// The sandbox boundary for command execution.
    pub sandbox: Confine,
    /// Named skill roots that may be mounted.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub skills: BTreeSet<SkillRootName>,
}

impl Authority {
    /// Whether this model may perform the requested operation.
    #[must_use]
    pub fn allows_model(&self, model: &ModelRef, mode: Mode) -> bool {
        self.models
            .get(model)
            .is_some_and(|modes| modes.contains(&mode))
    }

    /// Whether this tool may be exposed or called.
    #[must_use]
    pub fn allows_tool(&self, tool: &ToolName) -> bool {
        self.tools.contains(tool)
    }

    /// Whether this named skill root may be mounted.
    #[must_use]
    pub fn allows_skill(&self, root: &SkillRootName) -> bool {
        self.skills.contains(root)
    }

    /// Check that this authority is wholly contained in an approved ceiling.
    ///
    /// A failure names the native field that would widen it.
    pub fn fits_in(&self, ceiling: &Self) -> Result<()> {
        for (model, modes) in &self.models {
            let Some(ceiling_modes) = ceiling.models.get(model) else {
                return Err(
                    Error::denied(format!("model {model} is outside the envelope"))
                        .at("authority.models"),
                );
            };
            if !modes.is_subset(ceiling_modes) {
                return Err(Error::denied(format!(
                    "requested modes for model {model} exceed the envelope"
                ))
                .at("authority.models"));
            }
        }
        fits_names(
            self.tools.iter().map(ToolName::as_str),
            ceiling.tools.iter().map(ToolName::as_str),
            "authority.tools",
        )?;
        fits_names(
            self.skills.iter().map(SkillRootName::as_str),
            ceiling.skills.iter().map(SkillRootName::as_str),
            "authority.skills",
        )?;
        if !ceiling.sandbox.narrows_to(&self.sandbox) {
            return Err(
                Error::denied("sandbox confinement exceeds the envelope").at("authority.sandbox")
            );
        }
        Ok(())
    }

    /// Intersect two native capability sets.
    ///
    /// Sandbox confinements use [`Confine::meet`]; incomparable confinements are an error rather
    /// than an excuse to choose one and silently widen the other.
    pub fn meet(&self, other: &Self) -> Result<Self> {
        let models = self
            .models
            .iter()
            .filter_map(|(model, modes)| {
                other.models.get(model).map(|other_modes| {
                    let modes = modes.intersection(other_modes).copied().collect();
                    (model.clone(), modes)
                })
            })
            .collect();
        Ok(Self {
            models,
            tools: self.tools.intersection(&other.tools).cloned().collect(),
            sandbox: self.sandbox.meet(&other.sandbox)?,
            skills: self.skills.intersection(&other.skills).cloned().collect(),
        })
    }
}

fn fits_names<'a>(
    requested: impl Iterator<Item = &'a str>,
    ceiling: impl Iterator<Item = &'a str>,
    field: &str,
) -> Result<()> {
    let ceiling: BTreeSet<&str> = ceiling.collect();
    for outside in requested {
        if !ceiling.contains(outside) {
            return Err(Error::denied(format!("{outside} is outside the envelope")).at(field));
        }
    }
    Ok(())
}
