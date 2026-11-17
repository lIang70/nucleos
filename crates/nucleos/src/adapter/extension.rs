//! Provider-specific options, and how a caller reaches a knob the canonical vocabulary does not
//! model.
//!
//! An [`Extension`] is a *typed value*, not a map: a vendor's reasoning budget or safety level has a
//! name, a shape and a validator, and a typo fails validation instead of silently doing nothing. The
//! canonical request types stay the intersection of providers rather than the union.
//!
//! Two rules make it safe to carry several at once:
//!
//! - **An extension for another provider is inert.** A caller may carry options for several
//!   providers; each attempt applies only its own, and the rest are not errors.
//! - **An identity the running build does not know is an error.** A deployment that names an option
//!   this build cannot honour must not look configured.

use crate::contract::Field;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One provider-specific option set.
pub trait Extension: Send + Sync + std::fmt::Debug {
    /// Which provider this belongs to, e.g. `openai`.
    fn provider(&self) -> &str;
    /// Which extension within that provider, e.g. `generation`.
    fn id(&self) -> &str;
    /// The options this value actually sets, as names local to this extension.
    ///
    /// The qualified identity is `extension.<provider>.<id>.<option>`, which is how a dropped or
    /// ignored option becomes visible instead of silent.
    fn active_fields(&self) -> Vec<&'static str>;

    /// Downcast support for adapters consuming their own extension type.
    fn as_any(&self) -> &dyn std::any::Any;
    /// Check this value's own invariants.
    fn validate(&self) -> Result<()>;
    /// A deep copy.
    fn clone_box(&self) -> Box<dyn Extension>;
}

/// An ordered set of provider-specific options on one request.
#[derive(Debug, Default)]
pub struct ExtensionSet(Vec<Box<dyn Extension>>);

impl Clone for ExtensionSet {
    fn clone(&self) -> Self {
        Self(
            self.0
                .iter()
                .map(|extension| extension.clone_box())
                .collect(),
        )
    }
}

impl ExtensionSet {
    /// Build from values rather than from the wire.
    #[must_use]
    pub fn new(extensions: Vec<Box<dyn Extension>>) -> Self {
        Self(extensions)
    }

    /// Whether no options were given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every extension, in the order they were supplied.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Extension> {
        self.0.iter().map(Box::as_ref)
    }

    /// Only the extensions addressed to `provider`, preserving order.
    ///
    /// The rest are inert: a caller may carry options for several providers at once, and each
    /// attempt applies only its own.
    pub fn for_provider<'a>(
        &'a self,
        provider: &'a str,
    ) -> impl Iterator<Item = &'a dyn Extension> {
        self.iter()
            .filter(move |extension| extension.provider() == provider)
    }

    /// Every option this set claims, as qualified [`Field`] identities.
    #[must_use]
    pub fn active_fields(&self) -> Vec<Field> {
        self.iter()
            .flat_map(|extension| {
                extension
                    .active_fields()
                    .into_iter()
                    .map(|option| Field::Extension {
                        provider: extension.provider().to_owned(),
                        id: extension.id().to_owned(),
                        option: (*option).to_owned(),
                    })
            })
            .collect()
    }
}

impl PartialEq for ExtensionSet {
    fn eq(&self, other: &Self) -> bool {
        format!("{self:?}") == format!("{other:?}")
    }
}

/// The config-side reference to one extension: which provider's which extension, plus the fields a
/// deployment wrote. Decoding is strict: an unknown identity is an error, not an ignored value.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtensionEntry {
    /// The provider this belongs to.
    pub provider: String,
    /// The extension within that provider.
    pub id: String,
    /// The options the deployment wrote, as a JSON object.
    #[serde(default)]
    pub fields: serde_json::Value,
}

impl ExtensionEntry {
    /// The registry key an entry resolves against.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// Builds one typed extension from its wire-facing fields object.
pub type ExtensionDecoder = fn(&serde_json::Value) -> Result<Box<dyn Extension>>;

/// Resolve entries into typed extensions through the registered decoders.
///
/// `field` names the config location in errors (`inference node extensions`), so a caller can find
/// the offending entry rather than being told only that something was wrong.
pub fn decode_extensions(
    entries: &[ExtensionEntry],
    decoders: &BTreeMap<String, ExtensionDecoder>,
    field: &str,
) -> Result<ExtensionSet> {
    let mut extensions = Vec::with_capacity(entries.len());
    let mut seen = std::collections::BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let location = format!("{field}[{index}]");
        if entry.provider.is_empty() || entry.id.is_empty() {
            return Err(Error::invalid(format!(
                "{location}: provider and id are required"
            )));
        }
        let key = entry.key();
        if !seen.insert(key.clone()) {
            return Err(Error::invalid(format!(
                "{location}: duplicate extension {key:?}"
            )));
        }
        let Some(decoder) = decoders.get(&key) else {
            return Err(Error::invalid(format!(
                "{location}: extension {key:?} is not registered"
            )));
        };
        let extension = decoder(&entry.fields)
            .map_err(|error| Error::invalid(format!("{location}: {}", error.reason())))?;
        if extension.active_fields().is_empty() {
            return Err(Error::invalid(format!(
                "{location}: extension {key:?} sets no options"
            )));
        }
        extension
            .validate()
            .map_err(|error| Error::invalid(format!("{location}: {}", error.reason())))?;
        extensions.push(extension);
    }
    Ok(ExtensionSet::new(extensions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct StubExtension {
        provider: &'static str,
        id: &'static str,
        options: Vec<&'static str>,
        valid: bool,
    }

    impl Extension for StubExtension {
        fn provider(&self) -> &str {
            self.provider
        }
        fn id(&self) -> &str {
            self.id
        }
        fn active_fields(&self) -> Vec<&'static str> {
            self.options.clone()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn validate(&self) -> Result<()> {
            if self.valid {
                Ok(())
            } else {
                Err(Error::invalid("stub is invalid"))
            }
        }
        fn clone_box(&self) -> Box<dyn Extension> {
            Box::new(self.clone())
        }
    }

    fn stub(provider: &'static str, id: &'static str) -> Box<dyn Extension> {
        Box::new(StubExtension {
            provider,
            id,
            options: vec!["temperature"],
            valid: true,
        })
    }

    #[test]
    fn an_empty_set_is_empty_and_has_no_fields() {
        let set = ExtensionSet::default();
        assert!(set.is_empty());
        assert_eq!(set.iter().count(), 0);
        assert!(set.active_fields().is_empty());
    }

    #[test]
    fn for_provider_filters_and_preserves_order() {
        let set = ExtensionSet::new(vec![
            stub("openai", "generation"),
            stub("anthropic", "thinking"),
            stub("openai", "speech"),
        ]);
        let openai: Vec<_> = set.for_provider("openai").collect();
        assert_eq!(openai.len(), 2);
        assert_eq!(openai[0].id(), "generation");
        assert_eq!(openai[1].id(), "speech");
        // And the others are inert, not errors.
        assert_eq!(set.for_provider("anthropic").count(), 1);
        assert_eq!(set.for_provider("unknown").count(), 0);
    }

    #[test]
    fn active_fields_are_qualified_by_provider_and_id() {
        let set = ExtensionSet::new(vec![stub("openai", "generation")]);
        let fields = set.active_fields();
        assert_eq!(fields.len(), 1);
        let Field::Extension {
            provider,
            id,
            option,
        } = &fields[0]
        else {
            panic!("expected an Extension field");
        };
        assert_eq!(provider, "openai");
        assert_eq!(id, "generation");
        assert_eq!(option, "temperature");
    }

    #[test]
    fn entry_key_is_provider_slash_id() {
        let entry = ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({}),
        };
        assert_eq!(entry.key(), "openai/generation");
    }

    #[test]
    fn decoding_an_unregistered_identity_is_an_error_naming_the_location() {
        let entries = vec![ExtensionEntry {
            provider: "unknown".to_owned(),
            id: "thing".to_owned(),
            fields: serde_json::json!({}),
        }];
        let decoders = BTreeMap::new();
        let Err(error) = decode_extensions(&entries, &decoders, "inference node") else {
            panic!("unregistered must fail");
        };
        let message = error.to_string();
        assert!(message.contains("inference node[0]"));
        assert!(message.contains("unknown/thing"));
        assert!(message.contains("not registered"));
    }

    #[test]
    fn decoding_a_duplicate_is_an_error() {
        let entry = ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields: serde_json::json!({}),
        };
        let entries = vec![entry.clone(), entry];
        let mut decoders: BTreeMap<String, ExtensionDecoder> = BTreeMap::new();
        decoders.insert("openai/generation".to_owned(), |_fields| {
            Ok(stub("openai", "generation"))
        });
        let Err(error) = decode_extensions(&entries, &decoders, "node") else {
            panic!("duplicates must fail");
        };
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn decoding_an_extension_with_no_options_is_refused() {
        let entries = vec![ExtensionEntry {
            provider: "openai".to_owned(),
            id: "empty".to_owned(),
            fields: serde_json::json!({}),
        }];
        let mut decoders: BTreeMap<String, ExtensionDecoder> = BTreeMap::new();
        decoders.insert("openai/empty".to_owned(), |_fields| {
            Ok(Box::new(StubExtension {
                provider: "openai",
                id: "empty",
                options: Vec::new(),
                valid: true,
            }) as Box<dyn Extension>)
        });
        let Err(error) = decode_extensions(&entries, &decoders, "node") else {
            panic!("empty options must fail");
        };
        assert!(error.to_string().contains("sets no options"));
    }

    #[test]
    fn decoding_runs_the_extensions_own_validator() {
        let entries = vec![ExtensionEntry {
            provider: "openai".to_owned(),
            id: "bad".to_owned(),
            fields: serde_json::json!({}),
        }];
        let mut decoders: BTreeMap<String, ExtensionDecoder> = BTreeMap::new();
        decoders.insert("openai/bad".to_owned(), |_fields| {
            Ok(Box::new(StubExtension {
                provider: "openai",
                id: "bad",
                options: vec!["x"],
                valid: false,
            }) as Box<dyn Extension>)
        });
        let Err(error) = decode_extensions(&entries, &decoders, "node") else {
            panic!("an invalid extension must fail");
        };
        assert!(error.to_string().contains("stub is invalid"));
    }
}
