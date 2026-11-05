//! Where an administrator's private key lives: the operating system's keychain, or a file when there
//! is no keychain to speak of.
//!
//! The keychain is the right home for a *human's* secret. It is unlocked by a login session, it is
//! encrypted at rest by the OS, and it is not in a backup of the project directory. That is exactly
//! the situation of a person running `nucleos`.
//!
//! It is the wrong home for a **daemon**: a system service starts before anyone logs in, on macOS as
//! a LaunchDaemon with no user keychain, and on Linux as a system unit with no D-Bus session — the
//! two requirements of Secret Service. So the daemon holds no private key at all (§6 of
//! `docs/config.md`), and this module is only ever used by the CLI.
//!
//! Two backends, one interface:
//!
//! - **keychain** — `security` on macOS, `secret-tool` on Linux, invoked as a subprocess. No FFI, no
//!   `unsafe`, and the same code path a person would use by hand.
//! - **file** — `--key-out` for containers and CI, where there is no keychain and no login. A `0600`
//!   file with the raw seed, which is honest about being less protected than a keychain instead of
//!   pretending otherwise.

use nucleos::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Where a private key is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Store {
    /// The OS keychain, under a service name and an account name.
    Keychain {
        /// The service, e.g. `nucleos-admin`. One per deployment so two daemons do not collide.
        service: String,
        /// The account, usually the administrator's key id.
        account: String,
    },
    /// A file holding the raw seed, mode `0600`.
    File(PathBuf),
}

impl Store {
    /// The keychain entry for `account` under the default service.
    #[must_use]
    pub fn keychain(account: impl Into<String>) -> Self {
        Self::Keychain {
            service: "nucleos-admin".to_owned(),
            account: account.into(),
        }
    }

    /// Save a secret.
    pub fn put(&self, secret: &[u8]) -> Result<()> {
        match self {
            Self::Keychain { service, account } => keychain_put(service, account, secret),
            Self::File(path) => file_put(path, secret),
        }
    }

    /// Read a secret back.
    pub fn get(&self) -> Result<Vec<u8>> {
        match self {
            Self::Keychain { service, account } => keychain_get(service, account),
            Self::File(path) => std::fs::read(path).map_err(|error| {
                Error::invalid(format!(
                    "cannot read the key at {}: {error}",
                    path.display()
                ))
            }),
        }
    }

    /// Forget a secret. Absent counts as forgotten.
    pub fn delete(&self) -> Result<()> {
        match self {
            Self::Keychain { service, account } => {
                // Deleting something that is not there is not an error: revocation should be
                // idempotent, or a retry after a partial failure fails for the wrong reason.
                let _ = std::process::Command::new(keychain_program())
                    .args(keychain_delete_args(service, account))
                    .status();
                Ok(())
            }
            Self::File(path) => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(Error::invalid(format!(
                    "cannot remove the key at {}: {error}",
                    path.display()
                ))),
            },
        }
    }

    /// A description for the log line, never containing the secret.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Keychain { service, account } => {
                format!("keychain {service}/{account}")
            }
            Self::File(path) => format!("file {}", path.display()),
        }
    }
}

/// Whether this platform has a keychain this module knows how to talk to.
#[must_use]
pub fn available() -> bool {
    std::process::Command::new(keychain_program())
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success() || status.code().is_some())
}

fn file_put(path: &Path, secret: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::invalid(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    // Created with the mode, not chmod-ed afterwards: a window where the file is world-readable is a
    // window somebody else can read it in.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::invalid(format!("cannot write {}: {error}", path.display())))?;
    file.write_all(secret)
        .map_err(|error| Error::invalid(format!("cannot write {}: {error}", path.display())))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_program() -> &'static str {
    "security"
}

#[cfg(not(target_os = "macos"))]
fn keychain_program() -> &'static str {
    "secret-tool"
}

#[cfg(target_os = "macos")]
fn keychain_put(service: &str, account: &str, secret: &[u8]) -> Result<()> {
    // `security` takes the secret as an argument, which puts it in this process's argv for the
    // lifetime of the call. Reading it from stdin would be better and `security` does not offer it;
    // the window is microseconds, and the alternative is a hand-written keychain client.
    let hex = hex_encode(secret);
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            account,
            "-w",
            &hex,
        ])
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|error| Error::invalid(format!("cannot run `security`: {error}")))?;
    if !status.success() {
        return Err(Error::unavailable(format!(
            "the keychain refused to store an entry for {service}/{account}; is a keychain \
             unlocked in this session?"
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_get(service: &str, account: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .map_err(|error| Error::invalid(format!("cannot run `security`: {error}")))?;
    if !output.status.success() {
        return Err(Error::not_found(format!(
            "no key for {service}/{account} in the keychain"
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    decode_hex(text.trim())
}

#[cfg(target_os = "macos")]
fn keychain_delete_args(service: &str, account: &str) -> Vec<String> {
    vec![
        "delete-generic-password".to_owned(),
        "-s".to_owned(),
        service.to_owned(),
        "-a".to_owned(),
        account.to_owned(),
    ]
}

#[cfg(not(target_os = "macos"))]
fn keychain_put(service: &str, account: &str, secret: &[u8]) -> Result<()> {
    // `secret-tool` reads the secret from stdin, which is the behaviour `security` should have had.
    use std::io::Write as _;
    let mut child = std::process::Command::new("secret-tool")
        .args([
            "store", "--label", service, "service", service, "account", account,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|error| Error::unavailable(format!("cannot run `secret-tool`: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| Error::internal("secret-tool has no stdin"))?
        .write_all(&hex_encode(secret).into_bytes())
        .map_err(|error| Error::invalid(format!("cannot write to `secret-tool`: {error}")))?;
    let status = child
        .wait()
        .map_err(|error| Error::invalid(format!("`secret-tool` failed: {error}")))?;
    if !status.success() {
        return Err(Error::unavailable(format!(
            "the keychain refused to store an entry for {service}/{account}; is a Secret Service \
             (gnome-keyring, KWallet) running in this session?"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn keychain_get(service: &str, account: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("secret-tool")
        .args(["lookup", "service", service, "account", account])
        .output()
        .map_err(|error| Error::invalid(format!("cannot run `secret-tool`: {error}")))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(Error::not_found(format!(
            "no key for {service}/{account} in the keychain"
        )));
    }
    decode_hex(String::from_utf8_lossy(&output.stdout).trim())
}

#[cfg(not(target_os = "macos"))]
fn keychain_delete_args(service: &str, account: &str) -> Vec<String> {
    vec![
        "clear".to_owned(),
        "service".to_owned(),
        service.to_owned(),
        "account".to_owned(),
        account.to_owned(),
    ]
}

/// Hex, used by both backends: `security` takes the secret as an argument and `secret-tool` takes it
/// on stdin, and neither should be handed raw bytes with no encoding.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(Error::invalid(
            "the keychain entry is not hex-encoded bytes",
        ));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(&text[at..at + 2], 16)
                .map_err(|_| Error::invalid("the keychain entry is not hex-encoded bytes"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_file_backend_round_trips_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("nucleos-key-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::File(path.clone());

        store.put(&[7u8; 32]).unwrap();
        assert_eq!(store.get().unwrap(), vec![7u8; 32]);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "created with the mode, not chmod-ed after");

        store.delete().unwrap();
        store.delete().unwrap(); // idempotent: a second revocation is not an error
        assert!(store.get().is_err());
    }

    #[test]
    fn the_description_never_contains_the_secret() {
        let store = Store::keychain("7f3a1b2c");
        assert!(store.describe().contains("7f3a1b2c"));
        assert!(
            Store::File(PathBuf::from("/run/secrets/k"))
                .describe()
                .contains("/run/secrets/k")
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_odd_length() {
        assert_eq!(
            decode_hex(&hex_encode(&[0, 15, 255])).unwrap(),
            vec![0, 15, 255]
        );
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
