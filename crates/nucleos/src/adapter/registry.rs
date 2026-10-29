//! Named models, and route resolution.
//!
//! A plain struct, not a sixth trait. Routing needs *lookup*, and lookup with a fixed
//! implementation does not need polymorphism — adding a trait here would make the count six
//! without buying anything, and "five contracts" is a rule worth keeping true.

use crate::adapter::Model;
use crate::contract::Routing;
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The models a daemon can reach, by name.
#[derive(Default, Clone)]
pub struct ModelRegistry {
    models: BTreeMap<String, Arc<dyn Model>>,
}

impl ModelRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a model. Returns the previous one, if any.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        model: Arc<dyn Model>,
    ) -> Option<Arc<dyn Model>> {
        self.models.insert(name.into(), model)
    }

    /// Look one up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Model>> {
        self.models.get(name).cloned()
    }

    /// Every name, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// Resolve a route against what is registered.
    ///
    /// An unknown name is an error, not a skip. Skipping it would mean a typo in a route is
    /// discovered as "the model behaved differently", and by then the session has already run.
    pub fn resolve(&self, routing: &Routing) -> Result<Vec<(String, Arc<dyn Model>)>> {
        if routing.chain.is_empty() {
            return Err(Error::invalid(
                "routing chain is empty: a session needs at least one model",
            ));
        }
        let mut resolved = Vec::with_capacity(routing.chain.len());
        for name in &routing.chain {
            match self.models.get(name) {
                Some(model) => resolved.push((name.clone(), Arc::clone(model))),
                None => {
                    return Err(Error::not_found(format!(
                        "no model named {name:?}; known: {:?}",
                        self.names()
                    )));
                }
            }
        }
        Ok(resolved)
    }
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Model` is not `Debug`, and printing the names is what a reader wants anyway.
        f.debug_struct("ModelRegistry")
            .field("names", &self.names())
            .finish()
    }
}
