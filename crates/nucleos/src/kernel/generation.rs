//! Tool generations: an immutable set of tools, and the rule for moving a session between them.
//!
//! A tool source is a process somebody else deploys, so its tool list changes while sessions are
//! running. The kernel's answer is not to mutate a session's registry mid-turn — a call that has
//! been issued must resolve against the set that issued it — but to **publish** a new set and let
//! each session move when it reaches a turn boundary.
//!
//! Three rules make that safe:
//!
//! - **A session resolves tools through the generation it is pinned to.** Not the newest one: the
//!   model saw a prompt built from that set, so the call it produced has to be answered by it.
//! - **Compatibility is structural.** Same tool names, and input schemas that are structurally
//!   equal — a new *optional* property is fine, because a caller that does not send it is unaffected.
//!   A removed tool, a changed type or a new required property is not: the model may already have
//!   the old shape in mind.
//! - **Migration is a turn boundary event, never a mid-turn swap.** `generation_migrated` records
//!   it, because a replay has to know which set turn 42 ran under.
//!
//! **A generation is retired by forgetting its number, not by taking it away from anybody.** Sessions
//! hold their set (see `Pinned`), so dropping an entry frees it exactly when the last session pinned to
//! it ends: nobody can reach for a set that has been freed, because the set is only reachable through
//! a session. That is why retirement needs no scan of the store and no durable answer to "which
//! sessions are still alive" — the answer is the refcount, and it is already in the `Arc`.

use super::tool_search;
use crate::adapter::Tool;
use crate::contract::{EventKind, Generation, ToolSpec};
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// One immutable set of tools.
pub(crate) type ToolSet = BTreeMap<String, Arc<dyn Tool>>;

/// A set a session holds.
///
/// A newtype for one reason: `Session` is `Debug`, and a `BTreeMap<String, Arc<dyn Tool>>` is not.
/// The `Debug` here names the tools instead of the trait objects, which is also what a log wants.
#[derive(Clone)]
pub(crate) struct Pinned(Arc<ToolSet>);

impl Pinned {
    pub(crate) fn new(set: Arc<ToolSet>) -> Self {
        Self(set)
    }

    pub(crate) fn arc(&self) -> Arc<ToolSet> {
        Arc::clone(&self.0)
    }
}

impl std::ops::Deref for Pinned {
    type Target = ToolSet;
    fn deref(&self) -> &ToolSet {
        &self.0
    }
}

impl std::fmt::Debug for Pinned {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.0.keys()).finish()
    }
}

/// Every set this kernel still has a session for, plus the current one.
#[derive(Default)]
pub(crate) struct Generations {
    current: u64,
    sets: BTreeMap<u64, Arc<ToolSet>>,
}

impl Generations {
    /// Assemble the first generation from a builder's tools.
    pub(crate) fn first(tools: Vec<Arc<dyn Tool>>) -> Result<Self> {
        let mut generations = Self::default();
        generations.publish(tools)?;
        Ok(generations)
    }

    /// The generation new sessions are pinned to.
    pub(crate) fn current(&self) -> Generation {
        Generation(self.current)
    }

    /// The tools one generation holds, or the current one when the number is unknown.
    ///
    /// Unknown means a snapshot written by a daemon that had more generations than this one — a
    /// restore into a fresh process. Falling back to the current set is the only answer that lets
    /// the session run at all, and it is a *widening* of what the tool registry offers, so it is
    /// visible in `Describe` rather than silent.
    pub(crate) fn set(&self, generation: Generation) -> Arc<ToolSet> {
        self.sets
            .get(&generation.0)
            .or_else(|| self.sets.get(&self.current))
            .cloned()
            .unwrap_or_default()
    }

    /// The current tools.
    pub(crate) fn current_set(&self) -> Arc<ToolSet> {
        self.set(Generation(self.current))
    }

    /// Add a generation and make it current.
    ///
    /// Discovery is rebuilt for the new set: its catalog has to be these tools, not the ones that
    /// were registered when the kernel started.
    pub(crate) fn publish(&mut self, tools: Vec<Arc<dyn Tool>>) -> Result<Generation> {
        let mut set: ToolSet = BTreeMap::new();
        for tool in tools {
            let name = tool.spec().name;
            if set.contains_key(&name) {
                return Err(Error::invalid(format!(
                    "two tools are called {name:?}; a generation is a set, not a list"
                )));
            }
            set.insert(name, tool);
        }

        // The search tool is the kernel's, and it is per-generation because it indexes that
        // generation's catalog.
        if !set.is_empty() && !set.contains_key(tool_search::NAME) {
            let specs: Vec<ToolSpec> = set.values().map(|tool| tool.spec()).collect();
            let search: Arc<dyn Tool> = Arc::new(tool_search::Search::new(&specs));
            set.insert(tool_search::NAME.to_owned(), search);
        }

        self.current += 1;
        let generation = Generation(self.current);
        tracing::info!(
            event = "generation_published",
            generation = generation.0,
            tools = set.len(),
        );
        self.sets.insert(generation.0, Arc::new(set));
        Ok(generation)
    }

    /// Forget every set but the current one.
    ///
    /// Safe because a set is only reachable through a session that holds it: a session pinned to a
    /// retired generation keeps working, and can no longer be migrated *to* it — migration only goes to
    /// current, and current is never retired. Returns the numbers that were forgotten, so the caller
    /// can say what it did rather than that it did something.
    pub(crate) fn retire(&mut self) -> Vec<Generation> {
        let current = self.current;
        let retired: Vec<Generation> = self
            .sets
            .keys()
            .filter(|number| **number != current)
            .map(|number| Generation(*number))
            .collect();
        for generation in &retired {
            self.sets.remove(&generation.0);
        }
        retired
    }

    /// How many sets this kernel still knows about. What a retirement can be measured against.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.sets.len()
    }
}

/// Whether a session pinned to `pinned` may move to `next` at a turn boundary.
///
/// Structural, not textual: two schemas that mean the same thing are compared by what they require,
/// because the failure this prevents is a model sending a shape the tool no longer takes.
pub(crate) fn compatible(pinned: &ToolSet, next: &ToolSet) -> bool {
    // The search tool is the kernel's own and is rebuilt per generation; it is never the reason a
    // migration is refused.
    let names = |set: &ToolSet| -> BTreeSet<String> {
        set.keys()
            .filter(|name| *name != tool_search::NAME)
            .cloned()
            .collect()
    };

    if names(pinned) != names(next) {
        return false;
    }
    names(pinned).into_iter().all(|name| {
        let (Some(before), Some(after)) = (pinned.get(&name), next.get(&name)) else {
            return false;
        };
        schema_narrows(
            &before.spec().input_schema_json,
            &after.spec().input_schema_json,
        )
    })
}

/// Whether every request the old schema accepted is still accepted by the new one.
///
/// Adding an optional property is fine: nobody was sending it. Removing one, narrowing a type, or
/// making a previously optional property required is not — a model that learned the old shape would
/// produce a call the tool now refuses, and the refusal would be unexplainable from the prompt it
/// was working from.
fn schema_narrows(before: &str, after: &str) -> bool {
    let (Ok(before), Ok(after)) = (
        serde_json::from_str::<serde_json::Value>(before),
        serde_json::from_str::<serde_json::Value>(after),
    ) else {
        // A schema that is not JSON is a schema nobody can compare, so treating it as compatible
        // would be guessing in the permissive direction.
        return false;
    };
    if before == after {
        return true;
    }

    let required = |schema: &serde_json::Value| -> BTreeSet<String> {
        schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let properties = |schema: &serde_json::Value| -> BTreeMap<String, serde_json::Value> {
        schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Nothing new may be required, and nothing old may have changed meaning.
    if !required(&after).is_subset(&required(&before)) {
        return false;
    }
    let (before, after) = (properties(&before), properties(&after));
    before
        .iter()
        .all(|(name, schema)| after.get(name) == Some(schema))
}

/// Record a migration on the session, if one is due.
pub(crate) async fn migrate(
    kernel: &super::Kernel,
    session: &mut super::session::Session,
) -> Result<bool> {
    // Everything that needs the registry is answered before the first await: a read guard held
    // across one makes the whole turn non-`Send`, and the handlers that drive it stop compiling.
    let resolved = {
        let generations = kernel.generations();
        let current = generations.current();
        if session.generation == current {
            return Ok(false);
        }
        // The pinned side comes from the session itself, not from the map: a retired generation must
        // still be able to answer "am I compatible with the new one", or a session pinned to it would
        // silently be judged against itself.
        let (pinned, next) = (session.toolset.arc(), generations.current_set());
        if !compatible(&pinned, &next) {
            // Incompatible: the session stays where it is until it ends. Moving it would answer a
            // call with a tool the model was never shown.
            tracing::info!(
                event = "generation_incompatible",
                session = %session.id,
                pinned = session.generation.0,
                current = current.0,
            );
            return Ok(false);
        }
        (current, next)
    };

    let (current, next) = resolved;
    let from = session.generation;
    tracing::info!(
        event = "generation_migrated",
        session = %session.id,
        from = from.0,
        to = current.0,
    );
    session.generation = current;
    session.toolset = Pinned::new(next);
    kernel
        .emit(session, EventKind::GenerationMigrated { from, to: current })
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::adapter::{ToolCtx, ToolOutput};
    use async_trait::async_trait;

    struct Inert {
        name: String,
        schema: String,
    }

    #[async_trait]
    impl Tool for Inert {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: format!("{} tool", self.name),
                input_schema_json: self.schema.clone(),
            }
        }
        async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
            ToolOutput::text("ok")
        }
    }

    fn tool(name: &str, schema: &str) -> Arc<dyn Tool> {
        Arc::new(Inert {
            name: name.to_owned(),
            schema: schema.to_owned(),
        })
    }

    fn set(tools: Vec<Arc<dyn Tool>>) -> ToolSet {
        tools
            .into_iter()
            .map(|tool| (tool.spec().name, tool))
            .collect()
    }

    #[test]
    fn adding_an_optional_property_is_compatible() {
        let before = set(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
        )]);
        let after = set(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"}},"required":["path"]}"#,
        )]);
        assert!(compatible(&before, &after));
    }

    #[test]
    fn a_new_required_property_is_not() {
        let before = set(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{}}}"#,
        )]);
        let after = set(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{},"mode":{}},"required":["mode"]}"#,
        )]);
        assert!(
            !compatible(&before, &after),
            "a prompt built from the old shape cannot produce the new one"
        );
    }

    #[test]
    fn a_removed_or_retyped_tool_is_not() {
        let before = set(vec![tool("read_file", "{}"), tool("write_file", "{}")]);
        assert!(!compatible(&before, &set(vec![tool("read_file", "{}")])));

        let retyped = set(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{"type":"integer"}}}"#,
        )]);
        assert!(!compatible(
            &set(vec![tool(
                "read_file",
                r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
            )]),
            &retyped
        ));
    }

    #[test]
    fn the_search_tool_never_blocks_a_migration() {
        // It is rebuilt per generation, so its schema changes with the catalog.
        let before = set(vec![tool("read_file", "{}"), tool(tool_search::NAME, "{}")]);
        let after = set(vec![
            tool("read_file", "{}"),
            tool(tool_search::NAME, r#"{"x":1}"#),
        ]);
        assert!(compatible(&before, &after));
    }
}
