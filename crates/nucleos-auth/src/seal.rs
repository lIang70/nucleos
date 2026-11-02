//! Sealing: credentials the daemon must keep, encrypted with a key it must be given.
//!
//! A model's API key has to be stored somewhere, and the store is copied — backed up, restored on
//! another machine, attached to a support ticket. So the key that opens the store's sealed values lives
//! **outside** it, and the property this buys is one sentence long:
//!
//! > A copy of the data directory, on its own, contains no usable credential.
//!
//! Which is a narrower claim than "encrypted" and the honest one. A copy of the *whole machine*,
//! including the seal key, is not protected by this — that is what a machine-bound source is for, and
//! why the key can come from `systemd`'s credential directory (TPM-sealed, materialised in memory at
//! start-up) instead of a file.
//!
//! Sources, in this order, because each is strictly better than the one after it:
//!
//! 1. **A `systemd` credential** (`$CREDENTIALS_DIRECTORY/seal.key`). With
//!    `LoadCredentialEncrypted=seal.key:/etc/nucleos/seal.key.enc`, systemd decrypts a TPM-sealed blob at
//!    start-up and materialises it in a directory that only this service can read — the key itself is
//!    never on disk in the clear. Plain `LoadCredential=` also works and is only a file copy; the TPM
//!    property comes from the `Encrypted` form, and the difference is worth knowing before believing it.
//! 2. **The macOS System keychain** (`security find-generic-password`), for a LaunchDaemon whose
//!    deployment keeps the key there and is willing to manage the item's ACLs.
//! 3. **A `0600` file** at the configured path — the default, and the one that travels with a
//!    whole-machine backup.
//! 4. **Nothing** — no sealing; a model key can only be a file path, and the daemon says so when asked
//!    to store one.
//!
//! ChaCha20-Poly1305 rather than AES-GCM: constant-time in software without AES-NI, which is the common
//! case on the machines this runs on, and the nonce is generated per value rather than derived from a
//! counter that a crash could repeat.

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use nucleos::error::{Error, Result};
use std::path::{Path, PathBuf};

/// The environment variable `systemd` sets to the directory holding `LoadCredential=` files.
pub const CREDENTIALS_DIR_ENV: &str = "CREDENTIALS_DIRECTORY";
/// The file name looked for inside it.
pub const KEY_FILE: &str = "seal.key";

/// The key that seals values stored in the system store.
#[derive(Clone)]
pub struct SealKey {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No `Debug` of the material, for the same reason a private key has none.
        f.write_str("SealKey(<sealed>)")
    }
}

impl SealKey {
    /// A new key, from the operating system's randomness.
    pub fn generate() -> (Self, [u8; 32]) {
        use rand_core::RngCore as _;
        let mut bytes = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut bytes);
        (Self::from_bytes(&bytes), bytes)
    }

    /// A key from its 32 bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(bytes)),
        }
    }

    /// The sources to try, best first, for this deployment and platform.
    ///
    /// Built rather than probed inline so the order is a value a test can assert: a deployment that sets
    /// up `LoadCredentialEncrypted=` must not silently fall back to a weaker file that happens to exist,
    /// which is what "try in order until one works" means if the order is wrong.
    pub fn sources(configured: &Path) -> Vec<Source> {
        let mut sources = Vec::new();
        if let Some(directory) = std::env::var_os(CREDENTIALS_DIR_ENV) {
            sources.push(Source::Credential(PathBuf::from(directory).join(KEY_FILE)));
        }
        if cfg!(target_os = "macos") {
            sources.push(Source::Keychain {
                service: KEYCHAIN_SERVICE.to_owned(),
                account: KEYCHAIN_ACCOUNT.to_owned(),
            });
        }
        sources.push(Source::File(configured.to_path_buf()));
        sources
    }

    /// Where the key comes from, resolved, and **which source** answered.
    ///
    /// The source comes back with the key because "which one am I running" is the question an operator
    /// asks after moving the key to a stronger store: without it, a daemon that still read the old file
    /// looks exactly like one that read the new credential.
    pub fn resolve(configured: &Path) -> Result<Option<Resolved>> {
        Self::resolve_from(&Self::sources(configured))
    }

    /// Resolve from an explicit list, so the order can be tested without a systemd or a keychain.
    pub fn resolve_from(sources: &[Source]) -> Result<Option<Resolved>> {
        for source in sources {
            match source.read()? {
                Some(key) => {
                    return Ok(Some(Resolved {
                        key,
                        source: source.clone(),
                    }));
                }
                None => continue,
            }
        }
        Ok(None)
    }

    fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            Error::invalid(format!(
                "cannot read the seal key {}: {error}",
                path.display()
            ))
        })?;
        let bytes = decode_hex(text.trim()).map_err(|_| {
            Error::invalid(format!(
                "the seal key {} is not 64 hex characters",
                path.display()
            ))
        })?;
        let array: [u8; 32] = bytes.try_into().map_err(|_| {
            Error::invalid(format!("the seal key {} is not 32 bytes", path.display()))
        })?;
        Ok(Self::from_bytes(&array))
    }

    /// Seal one value. The result carries its own nonce and is safe to store.
    pub fn seal(&self, plaintext: &str) -> Result<String> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| Error::internal("cannot seal the value"))?;
        let mut framed = nonce.to_vec();
        framed.extend_from_slice(&ciphertext);
        Ok(framed.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    /// Open a value this key sealed. Anything else is a refusal, not a guess.
    pub fn open(&self, sealed: &str) -> Result<String> {
        let framed =
            decode_hex(sealed).map_err(|_| Error::invalid("a sealed value is not hex-encoded"))?;
        if framed.len() <= 12 {
            return Err(Error::invalid(
                "a sealed value is too short to hold a nonce",
            ));
        }
        let (nonce, ciphertext) = framed.split_at(12);
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| {
                // Wrong key or tampered value — the two are indistinguishable, and both mean "do not
                // use this".
                Error::denied("this value was sealed with a different key, or has been altered")
            })?;
        String::from_utf8(plaintext)
            .map_err(|_| Error::invalid("the sealed value is not UTF-8; it was not a string"))
    }
}

/// A key, and where it came from.
#[derive(Debug)]
pub struct Resolved {
    /// The key itself.
    pub key: SealKey,
    /// The source that answered.
    pub source: Source,
}

/// One place a key might be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file `systemd` materialised from a `LoadCredential[Encrypted]=` declaration.
    Credential(PathBuf),
    /// A macOS keychain item: `service` and `account`, as `security` names them.
    Keychain {
        /// The service name.
        service: String,
        /// The account within it.
        account: String,
    },
    /// A file the operator protects.
    File(PathBuf),
}

impl Source {
    /// Read this source. `Ok(None)` means "not configured here", not "broken".
    pub fn read(&self) -> Result<Option<SealKey>> {
        match self {
            Self::Credential(path) | Self::File(path) => {
                // A credential directory exists even when the named file was never declared, so absence
                // is the normal case and not an error.
                if !path.exists() {
                    return Ok(None);
                }
                SealKey::read(path).map(Some)
            }
            Self::Keychain { service, account } => keychain(service, account),
        }
    }

    /// A description for the log line, never containing the key.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Credential(path) | Self::File(path) => path.display().to_string(),
            Self::Keychain { service, account } => format!("keychain {service}/{account}"),
        }
    }
}

/// The keychain entry a macOS deployment would create.
const KEYCHAIN_SERVICE: &str = "nucleos-seal";
/// One item per deployment; the account names it.
const KEYCHAIN_ACCOUNT: &str = "seal";

/// Read the seal key from the macOS keychain.
///
/// `security` rather than a keychain crate: the daemon needs to read one generic password, and a
/// dependency that wraps the same command is a dependency with a supply chain. On any other platform
/// this is not a source at all, so it compiles away.
#[cfg(target_os = "macos")]
fn keychain(service: &str, account: &str) -> Result<Option<SealKey>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|error| Error::invalid(format!("cannot run `security`: {error}")))?;
    if !output.status.success() {
        // 44 is "item not found", which is a deployment that does not use the keychain.
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let bytes = decode_hex(text.trim())
        .map_err(|_| Error::invalid("the keychain item is not 64 hex characters"))?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::invalid("the keychain item is not 32 bytes"))?;
    Ok(Some(SealKey::from_bytes(&array)))
}

#[cfg(not(target_os = "macos"))]
fn keychain(_service: &str, _account: &str) -> Result<Option<SealKey>> {
    Ok(None)
}

/// Write a key to disk, `0600`, created with that mode rather than chmod-ed afterwards.
pub fn write_key(path: &Path, bytes: &[u8; 32]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::invalid(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::invalid(format!("cannot write {}: {error}", path.display())))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    file.write_all(hex.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| Error::invalid(format!("cannot write {}: {error}", path.display())))
}

fn decode_hex(text: &str) -> std::result::Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) {
        return Err(());
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_sealed_value_round_trips_and_says_nothing_about_itself() {
        let (key, _) = SealKey::generate();
        let sealed = key.seal("sk-a-real-looking-api-key").unwrap();
        assert!(!sealed.contains("sk-"), "{sealed}");
        assert_eq!(key.open(&sealed).unwrap(), "sk-a-real-looking-api-key");
    }

    #[test]
    fn two_sealings_of_one_value_differ() {
        // A per-value nonce: two rows holding the same key must not look identical, and a counter would
        // repeat after a crash.
        let (key, _) = SealKey::generate();
        assert_ne!(key.seal("same").unwrap(), key.seal("same").unwrap());
    }

    #[test]
    fn another_key_cannot_open_it_and_neither_can_a_modified_value() {
        let (mine, _) = SealKey::generate();
        let (theirs, _) = SealKey::generate();
        let sealed = mine.seal("secret").unwrap();

        let error = theirs.open(&sealed).unwrap_err();
        assert!(error.reason().contains("different key"), "{error:?}");

        // Flip one character of the ciphertext: the tag catches it.
        let mut tampered = sealed.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert!(mine.open(&tampered).is_err());
    }

    #[test]
    fn a_key_file_is_hex_and_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("nucleos-seal-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (key, bytes) = SealKey::generate();
        write_key(&path, &bytes).unwrap();

        let reloaded = SealKey::resolve(&path).unwrap().expect("the file is there");
        // The source comes back with the key: a test can assert *which* one answered, which is the
        // question an operator asks after moving the key somewhere stronger.
        assert_eq!(reloaded.source, Source::File(path.clone()));
        let sealed = key.seal("api-key").unwrap();
        assert_eq!(reloaded.key.open(&sealed).unwrap(), "api-key");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_source_means_no_sealing_rather_than_a_default_key() {
        // A built-in key would be worse than none: it would look like encryption and be a constant.
        let missing = std::env::temp_dir().join("nucleos-no-such-seal-key");
        let _ = std::fs::remove_file(&missing);
        assert!(SealKey::resolve(&missing).unwrap().is_none());
    }

    #[test]
    fn a_malformed_key_file_is_refused_by_name() {
        let path = std::env::temp_dir().join(format!("nucleos-seal-bad-{}", std::process::id()));
        std::fs::write(&path, "not hex at all").unwrap();
        let error = SealKey::resolve(&path).unwrap_err();
        assert!(error.reason().contains("seal key"), "{error:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_sources_are_ordered_best_first() {
        // The order is the design: a deployment that set up a TPM-sealed credential must not silently
        // fall back to a file that happens to exist. Asserted on the list rather than on a probe, because
        // a probe can only be run on one platform at a time.
        let configured = std::path::Path::new("/etc/nucleos/seal.key");
        let sources = SealKey::sources(configured);
        // Whatever the platform, the file is last: it is the weakest source and the fallback.
        assert!(matches!(sources.last(), Some(Source::File(path)) if path == configured));
        // And nothing appears twice.
        for (at, source) in sources.iter().enumerate() {
            assert!(!sources[..at].contains(source), "{sources:?}");
        }
    }

    #[test]
    fn a_missing_source_is_skipped_rather_than_fatal() {
        let missing = Source::Credential(PathBuf::from("/nonexistent/creds/seal.key"));
        assert!(missing.read().unwrap().is_none());
        assert!(SealKey::resolve_from(&[missing]).unwrap().is_none());
    }

    #[test]
    fn the_keychain_is_not_a_source_off_macos() {
        let source = Source::Keychain {
            service: "nucleos-seal".to_owned(),
            account: "seal".to_owned(),
        };
        // On this platform it must answer "not configured" rather than fail: a deployment that names a
        // keychain item on Linux has simply configured a source that does not exist here.
        assert!(source.read().unwrap().is_none());
        assert!(source.describe().contains("keychain"));
    }
}
