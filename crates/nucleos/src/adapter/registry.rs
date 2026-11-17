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

    /// Check every registered model's declaration.
    ///
    /// The one place that can answer "declared but uncallable": a model whose declaration the kernel
    /// would refuse on every call is a deployment error, and finding out per turn means finding out
    /// in front of a user. Failing at construction turns it into a config error with a name on it.
    ///
    /// # Errors
    ///
    /// The first model whose capability does not validate, named.
    pub fn validate(&self) -> Result<()> {
        for (name, model) in &self.models {
            model
                .capability()
                .validate()
                .map_err(|reason| Error::invalid(format!("model {name:?}: {reason}")))?;
        }
        Ok(())
    }

    /// Look one up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Model>> {
        self.models.get(name).cloned()
    }

    /// Every name, sorted.
    #[must_use]
    /// Forget a model. `true` if it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.models.remove(name).is_some()
    }

    /// The registered names, sorted.
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
                    return Err(Error::new(
                        crate::contract::ErrorCode::UnknownModel,
                        format!("no model named {name:?}; known: {:?}", self.names()),
                    ));
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::adapter::ModelStream;
    use crate::contract::{Capability, Routing};
    use async_trait::async_trait;

    struct Stub;

    #[async_trait]
    impl Model for Stub {
        fn capability(&self) -> Capability {
            Capability {
                inputs: Vec::new(),
                outputs: Vec::new(),
                tool_calling: true,
                tool_choice_required: true,
                structured_output: true,
                context_window: 100_000,
                max_output_tokens: 4_096,
                max_input_bytes: None,
            }
        }
        fn count_tokens(&self, _request: &crate::adapter::ModelRequest) -> Result<u64> {
            Ok(1)
        }
        fn provider_id(&self) -> &'static str {
            "stub"
        }
        fn prepare(
            &self,
            _request: crate::adapter::ModelRequest,
        ) -> Result<crate::adapter::Prepared> {
            unimplemented!("not used in registry tests")
        }
        async fn invoke(&self, _prepared: crate::adapter::Prepared) -> Result<ModelStream> {
            unimplemented!("not used in registry tests")
        }
    }

    fn registry_with(names: &[&str]) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        for name in names {
            registry.insert(*name, Arc::new(Stub));
        }
        registry
    }

    #[test]
    fn an_empty_chain_is_refused_before_any_lookup() {
        let registry = registry_with(&["a", "b"]);
        let routing = Routing {
            chain: Vec::new(),
            retries: 0,
        };
        let Err(error) = registry.resolve(&routing) else {
            panic!("an empty chain must not resolve");
        };
        assert!(error.to_string().contains("routing chain is empty"));
    }

    #[test]
    fn an_unknown_name_is_an_error_that_lists_the_known_ones() {
        let registry = registry_with(&["alpha", "beta"]);
        let routing = Routing {
            chain: vec!["alpha".to_owned(), "gamma".to_owned()],
            retries: 0,
        };
        let Err(error) = registry.resolve(&routing) else {
            panic!("an unknown model must not resolve");
        };
        let message = error.to_string();
        assert!(message.contains("gamma"), "names the missing model");
        assert!(message.contains("alpha"), "lists what is available");
        assert!(message.contains("beta"));
    }

    #[test]
    fn a_resolved_chain_preserves_the_order_the_caller_wrote() {
        let registry = registry_with(&["a", "b", "c"]);
        let routing = Routing {
            chain: vec!["c".to_owned(), "a".to_owned(), "b".to_owned()],
            retries: 0,
        };
        let resolved = registry.resolve(&routing).unwrap();
        let names: Vec<&str> = resolved.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
    }

    #[test]
    fn a_duplicate_name_resolves_twice_to_the_same_model() {
        let registry = registry_with(&["a"]);
        let routing = Routing {
            chain: vec!["a".to_owned(), "a".to_owned()],
            retries: 0,
        };
        let resolved = registry.resolve(&routing).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(Arc::ptr_eq(&resolved[0].1, &resolved[1].1));
    }

    #[test]
    fn names_are_sorted_regardless_of_insertion_order() {
        let registry = registry_with(&["zeta", "alpha", "mu"]);
        assert_eq!(registry.names(), ["alpha", "mu", "zeta"]);
    }

    #[test]
    fn removing_a_model_makes_it_unresolvable() {
        let mut registry = registry_with(&["a", "b"]);
        assert!(registry.remove("a"));
        assert!(!registry.remove("a"), "second remove finds nothing");
        let routing = Routing {
            chain: vec!["a".to_owned()],
            retries: 0,
        };
        assert!(registry.resolve(&routing).is_err());
        // And the survivor still resolves.
        let routing = Routing {
            chain: vec!["b".to_owned()],
            retries: 0,
        };
        assert!(registry.resolve(&routing).is_ok());
    }
}
