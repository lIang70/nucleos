//! `nucleos config`: read back what a deployment is running.
//!
//! The first command that presents the key `nucleos init` created, and a read deliberately: the
//! mechanism that proves who is asking arrived before the surface that changes anything, so an
//! administrator can confirm the policy, the enrolled keys and the generation in force without a
//! mutation existing yet.
//!
//! Where the pieces come from is [`crate::remote`]'s business; what is left here is the client config
//! `init` writes and the one-line `key=value` format that a person may want to read.

use crate::remote::{Overrides, Remote};
use nucleos::error::{Error, Result};
use std::path::{Path, PathBuf};

/// What the operator said.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Where the daemon listens.
    pub socket: Option<PathBuf>,
    /// Which key signs.
    pub key_id: Option<String>,
    /// A file holding the private key, for a machine without a keychain.
    pub key_file: Option<PathBuf>,
    /// The client config written by `init`.
    pub client_config: Option<PathBuf>,
}

impl Options {
    fn overrides(&self) -> Overrides {
        Overrides {
            socket: self.socket.clone(),
            key_id: self.key_id.clone(),
            key_file: self.key_file.clone(),
            client_config: self.client_config.clone(),
        }
    }
}

/// The client config, as `init` writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientConfig {
    /// Where the daemon listens.
    pub socket: Option<PathBuf>,
    /// Which key signs for this client.
    pub key: Option<String>,
}

impl ClientConfig {
    /// Read the file, if there is one.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // Absent is a state, not an error: a container may pass `--socket` and `--key-file`.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(Error::invalid(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self::parse(&text))
    }

    /// `key=value` lines, blank lines and `#` comments ignored.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "socket" => config.socket = Some(PathBuf::from(value.trim())),
                "key" => config.key = Some(value.trim().to_owned()),
                // An unknown key is ignored rather than refused: a newer client may write something
                // this build does not know, and failing to read the two lines it does know would be a
                // version incompatibility in the wrong direction.
                _ => {}
            }
        }
        config
    }
}

/// Read what the daemon is running, and print it.
pub fn run(options: &Options) -> Result<()> {
    println!(
        "{}",
        Remote::open(&options.overrides())?.call("GET", "/v1/config", b"")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_client_config_is_two_lines_a_person_can_edit() {
        let config = ClientConfig::parse(
            "# written by nucleos init\nsocket=/run/nucleos/nucleosd.sock\nkey=87af1de5\n",
        );
        assert_eq!(
            config.socket,
            Some(PathBuf::from("/run/nucleos/nucleosd.sock"))
        );
        assert_eq!(config.key.as_deref(), Some("87af1de5"));
    }

    #[test]
    fn an_unknown_line_is_ignored_rather_than_refused() {
        // A newer client writing a third line must not stop an older one from finding the first two.
        let config = ClientConfig::parse("socket=/s\ncolour=blue\nkey=k\n");
        assert_eq!(config.socket, Some(PathBuf::from("/s")));
        assert_eq!(config.key.as_deref(), Some("k"));
    }

    #[test]
    fn a_missing_file_is_an_empty_config_not_an_error() {
        let config = ClientConfig::load(Path::new("/nonexistent/nucleos/socket")).unwrap();
        assert_eq!(config, ClientConfig::default());
    }

    #[test]
    fn nothing_configured_names_what_to_do_instead() {
        let error = run(&Options::default()).unwrap_err();
        assert!(error.reason().contains("--socket"), "{error:?}");
    }
}
