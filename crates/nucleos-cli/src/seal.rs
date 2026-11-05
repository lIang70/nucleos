//! `nucleos seal rotate`: re-seal every stored credential under a new key.
//!
//! Rotation is two steps, and the second one is a restart. The daemon resolves its seal key at
//! start-up and keeps it; hot-swapping it would leave a window where some rows are openable by the new
//! key and some by the old, and nothing in the system would say which. So: stop, rotate, start.
//!
//! Two phases, in this order, because the alternative is a half-rotated store:
//!
//! 1. **Open every sealed row with the current key.** If one fails, nothing has been written — a store
//!    that is still entirely under the old key is a store that still works.
//! 2. **Write every row back under the new key**, then the new key itself, then the record.
//!
//! The rows are model API keys; there are few of them, so holding the plaintexts for the duration of
//! one command is cheaper than any scheme that would make the two phases interleavable.

use nucleos::error::{Error, Result};
use nucleos_auth::seal::{self, SealKey, Source};
use nucleos_sqlite::{Change, SystemStore};
use std::path::{Path, PathBuf};

/// Where the daemon keeps a model's sealed key. The same prefix the daemon uses.
pub const SEALED_KEY_PREFIX: &str = "config/keys/model/";

/// What to rotate.
#[derive(Debug, Clone)]
pub struct Options {
    /// The data directory.
    pub root: PathBuf,
    /// Where the *current* key is looked for: a credential directory first, then this path.
    pub current: PathBuf,
    /// Where the new key goes. Must be outside the root, for the same reason `init` refuses one inside
    /// it: a key beside the ciphertext it opens makes the sealing decorative.
    pub next: PathBuf,
}

/// What rotation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// How many stored credentials were re-sealed.
    pub rotated: usize,
    /// Their model names.
    pub models: Vec<String>,
    /// Where the new key went.
    pub key_path: PathBuf,
    /// The old key's source, so the operator knows what to delete or revoke.
    pub previous: String,
}

/// Re-seal everything under a new key.
pub async fn rotate(options: &Options) -> Result<Done> {
    if options.next.starts_with(&options.root) {
        return Err(Error::invalid(format!(
            "the new seal key must live outside the data directory: {} is inside {}",
            options.next.display(),
            options.root.display()
        )));
    }

    let system = SystemStore::open(options.root.join("system.db"))?;
    let rows = system.with_prefix(SEALED_KEY_PREFIX).await?;

    // Phase 1: open everything, or write nothing.
    let current = SealKey::resolve(&options.current)?;
    let mut plaintexts = Vec::with_capacity(rows.len());
    for (key, sealed) in &rows {
        let Some(resolved) = &current else {
            return Err(Error::unavailable(
                "there are sealed credentials but no seal key: pass the current one with --seal-key",
            ));
        };
        let model = key.strip_prefix(SEALED_KEY_PREFIX).unwrap_or(key);
        let plaintext = resolved.key.open(sealed).map_err(|error| {
            Error::invalid(format!(
                "cannot open the credential for {model:?} with {}: {}",
                resolved.source.describe(),
                error.reason()
            ))
        })?;
        plaintexts.push((key.clone(), model.to_owned(), plaintext));
    }

    let previous = current
        .as_ref()
        .map(|resolved| resolved.source.describe())
        .unwrap_or_else(|| "none".to_owned());

    // Phase 2: write the new rows, then the key, then the record.
    let (next, bytes) = SealKey::generate();
    for (key, _, plaintext) in &plaintexts {
        system.set(key, &next.seal(plaintext)?).await?;
    }
    seal::write_key(&options.next, &bytes)?;

    // Verified by reading back what was just written: a rotation that reports success and leaves a
    // value the new key cannot open is a deployment that loses its models on the next boot.
    for (key, model, _) in &plaintexts {
        let stored = system
            .get(key)
            .await?
            .ok_or_else(|| Error::internal(format!("{model:?} vanished during rotation")))?;
        next.open(&stored).map_err(|error| {
            Error::internal(format!(
                "{model:?} does not open with the new key: {}",
                error.reason()
            ))
        })?;
    }

    let models: Vec<String> = plaintexts.into_iter().map(|(_, model, _)| model).collect();
    system
        .record(Change {
            at_unix_millis: now(),
            tenant: "system".to_owned(),
            subject: "seal".to_owned(),
            key_id: String::new(),
            key: "seal".to_owned(),
            value: format!("rotated {} credential(s)", models.len()),
        })
        .await?;

    tracing::info!(event = "seal_rotated", credentials = models.len(), key = %options.next.display());

    Ok(Done {
        rotated: models.len(),
        models,
        key_path: options.next.clone(),
        previous,
    })
}

/// The sources a rotation should look through, best first — the same order the daemon uses, so a
/// rotation cannot succeed against a key the daemon would never have read.
#[must_use]
pub fn sources(current: &Path) -> Vec<Source> {
    SealKey::sources(current)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nucleos-rotate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    async fn seeded(root: &Path, key: &Path, models: &[(&str, &str)]) -> SealKey {
        std::fs::create_dir_all(root).unwrap();
        let (seal, bytes) = SealKey::generate();
        seal::write_key(key, &bytes).unwrap();
        let system = SystemStore::open(root.join("system.db")).unwrap();
        for (name, secret) in models {
            system
                .set(
                    &format!("{SEALED_KEY_PREFIX}{name}"),
                    &seal.seal(secret).unwrap(),
                )
                .await
                .unwrap();
        }
        seal
    }

    #[tokio::test]
    async fn rotation_re_seals_every_credential_under_a_new_key() {
        let root = scratch("ok");
        let old = root.with_extension("old.key");
        let new = root.with_extension("new.key");
        let before = seeded(&root, &old, &[("primary", "sk-one"), ("cheap", "sk-two")]).await;

        let done = rotate(&Options {
            root: root.clone(),
            current: old.clone(),
            next: new.clone(),
        })
        .await
        .unwrap();
        assert_eq!(done.rotated, 2);
        // Key order, not insertion order: the listing is deterministic, which is what a rotation
        // report needs to be comparable between runs.
        assert_eq!(done.models, ["cheap", "primary"]);

        // The new key opens both; the old one opens neither.
        let after = SealKey::resolve(&new)
            .unwrap()
            .expect("the new key is on disk");
        let system = SystemStore::open(root.join("system.db")).unwrap();
        for name in ["primary", "cheap"] {
            let stored = system
                .get(&format!("{SEALED_KEY_PREFIX}{name}"))
                .await
                .unwrap()
                .unwrap();
            assert!(after.key.open(&stored).is_ok(), "{name}");
            assert!(
                before.open(&stored).is_err(),
                "{name} must not open under the old key"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(&new);
    }

    #[tokio::test]
    async fn a_key_that_cannot_open_the_store_changes_nothing() {
        // The two-phase order: a failed first phase must leave the store entirely under the old key,
        // which is a store that still works.
        let root = scratch("wrong");
        let old = root.with_extension("old.key");
        let new = root.with_extension("new.key");
        let _ = seeded(&root, &old, &[("primary", "sk-one")]).await;
        let (stranger, stranger_bytes) = SealKey::generate();
        let wrong = root.with_extension("wrong.key");
        seal::write_key(&wrong, &stranger_bytes).unwrap();

        let error = rotate(&Options {
            root: root.clone(),
            current: wrong.clone(),
            next: new.clone(),
        })
        .await
        .unwrap_err();
        assert!(error.reason().contains("cannot open"), "{error:?}");
        assert!(!new.exists(), "no new key was written");

        // The original key still opens the row, because nothing was touched.
        let system = SystemStore::open(root.join("system.db")).unwrap();
        let stored = system
            .get(&format!("{SEALED_KEY_PREFIX}primary"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            stranger.open(&stored).is_err(),
            "the wrong key opens nothing"
        );
        let original = SealKey::resolve(&old).unwrap().expect("the original key");
        assert_eq!(original.key.open(&stored).unwrap(), "sk-one");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(&wrong);
    }

    #[tokio::test]
    async fn a_new_key_inside_the_data_directory_is_refused() {
        let root = scratch("inside");
        let old = root.with_extension("old.key");
        let _ = seeded(&root, &old, &[("primary", "sk-one")]).await;

        let error = rotate(&Options {
            root: root.clone(),
            current: old.clone(),
            next: root.join("seal.key"),
        })
        .await
        .unwrap_err();
        assert!(
            error.reason().contains("outside the data directory"),
            "{error:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&old);
    }

    #[tokio::test]
    async fn a_store_with_nothing_sealed_rotates_to_nothing() {
        let root = scratch("empty");
        std::fs::create_dir_all(&root).unwrap();
        SystemStore::open(root.join("system.db")).unwrap();
        let new = root.with_extension("new.key");

        let done = rotate(&Options {
            root: root.clone(),
            current: root.with_extension("absent.key"),
            next: new.clone(),
        })
        .await
        .unwrap();
        assert_eq!(done.rotated, 0);
        assert!(new.exists(), "a key is written so the deployment has one");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&new);
    }
}
