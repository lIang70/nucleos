//! `nucleos config`: read, export, apply and audit a deployment's configuration.
//!
//! `show` presents the key `nucleos init` created and prints what the daemon is running. `export`
//! writes the whole document with its revision, `apply` sends one back (optionally `--if-match` a
//! revision, which turns the write into a compare-and-swap), and `history` lists who changed what
//! with which key — the record the daemon appended, not a summary of it.
//!
//! Where the pieces come from is [`crate::remote`]'s business; what is left here is the client config
//! `init` writes and the one-line `key=value` format that a person may want to read.

use crate::remote::{Overrides, Remote};
use nucleos::error::{Error, Result};
use std::path::{Path, PathBuf};

/// What the operator asked `nucleos config` to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Command {
    /// Print what the daemon is running.
    #[default]
    Show,
    /// Write the whole configuration as one document.
    Export {
        /// Where to write it. Absent writes to standard output.
        out: Option<PathBuf>,
    },
    /// Replace the configuration from a document.
    Apply {
        /// The document. `-` reads standard input.
        file: String,
        /// Only if the deployment is still at this revision.
        if_match: Option<u64>,
    },
    /// What has been changed, and by which key.
    History {
        /// How many rows.
        limit: usize,
    },
}

/// What the operator said.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The subcommand.
    pub command: Command,
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
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
    let remote = Remote::open(&options.overrides())?;
    match &options.command {
        Command::Show => {
            println!("{}", remote.call("GET", "/v1/config", b"")?);
        }
        Command::Export { out } => {
            let raw = remote.call("GET", "/v1/config", b"")?;
            let document = document_from(&raw)?;
            let text = serde_json::to_string_pretty(&document)
                .map_err(|error| Error::internal(format!("cannot write the document: {error}")))?;
            match out {
                Some(path) => {
                    std::fs::write(path, format!("{text}\n")).map_err(|error| {
                        Error::invalid(format!("cannot write {}: {error}", path.display()))
                    })?;
                    // Said on stdout rather than in the file: a document with a comment in it is a
                    // document no parser accepts.
                    println!("{}", path.display());
                }
                None => println!("{text}"),
            }
        }
        Command::Apply { file, if_match } => {
            let body = if file == "-" {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map_err(
                    |error: std::io::Error| {
                        Error::invalid(format!("cannot read the document from stdin: {error}"))
                    },
                )?;
                text
            } else {
                std::fs::read_to_string(file)
                    .map_err(|error| Error::invalid(format!("cannot read {file}: {error}")))?
            };
            // Parsed here so a typo is a local error rather than a 400 from the daemon.
            serde_json::from_str::<serde_json::Value>(&body).map_err(|error| {
                Error::invalid(format!("{file} is not a JSON document: {error}"))
            })?;
            let header = if_match.map(|revision| revision.to_string());
            let extra: Vec<(&str, &str)> = header
                .as_deref()
                .map(|value| vec![("if-match", value)])
                .unwrap_or_default();
            let answer = remote.call_with("PUT", "/v1/config", body.as_bytes(), &extra)?;
            let applied: serde_json::Value = serde_json::from_str(&answer)
                .map_err(|error| Error::internal(format!("the daemon answered oddly: {error}")))?;
            println!(
                "{}",
                applied["summary"].as_str().unwrap_or("applied").to_owned()
                    + &format!(" (revision {})", applied["revision"])
            );
        }
        Command::History { limit } => {
            let raw = remote.call("GET", &format!("/v1/config/history?limit={limit}"), b"")?;
            let answer: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|error| Error::internal(format!("the daemon answered oddly: {error}")))?;
            for change in answer["changes"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:>13}  {:<12}  {:<18}  {}",
                    change["at_unix_millis"],
                    change["key_id"].as_str().unwrap_or("-"),
                    change["key"].as_str().unwrap_or("-"),
                    change["value"].as_str().unwrap_or(""),
                );
            }
        }
    }
    Ok(())
}

/// Turn the daemon's answer into a document that can be applied back.
///
/// The two shapes differ on purpose: `GET /v1/config` also reports things that are not configuration
/// (the schema version, where the data lives, the tool generation), and an export that carried them
/// would be a file you cannot re-apply without editing.
fn document_from(raw: &str) -> Result<nucleos::wire::api::Deployment> {
    let answer: nucleos::wire::api::ConfigResponse = serde_json::from_str(raw)
        .map_err(|error| Error::internal(format!("the daemon answered oddly: {error}")))?;
    Ok(nucleos::wire::api::Deployment {
        document_version: answer.document_version,
        revision: answer.revision,
        policy: answer.policy_json,
        models: answer.models,
        sources: answer.sources,
        tenants: answer.tenants,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The daemon's answer carries what an export must not: schema version, root, generation. The
    /// document is the part that can be applied back, and nothing else.
    #[test]
    fn an_export_is_only_the_part_that_can_be_applied_back() {
        let answer = serde_json::json!({
            "schema_version": 1,
            "root": "/var/lib/nucleos",
            "generation": 7,
            "document_version": 1,
            "revision": 4,
            "policy_json": "{\"default\":\"deny\",\"rules\":[]}",
            "models": [{"name": "primary", "url": "https://example.invalid/v1", "model": "m",
                        "context_window": 128000}],
            "sources": [],
            "tenants": {"acme": {"limits": {"tenant": "acme", "tools": ["read_file"]}}},
            "admin_keys": [],
            "tenant_policies": ["acme"],
            "signed_body_bytes": 0
        })
        .to_string();

        let document = document_from(&answer).unwrap();
        assert_eq!(document.document_version, 1);
        assert_eq!(document.revision, 4);
        assert_eq!(document.models.len(), 1);
        assert_eq!(
            document.tenants["acme"].limits.as_ref().unwrap().tools,
            Some(vec!["read_file".to_owned()])
        );

        // Serialising it gives a document the daemon accepts: no `schema_version`, no `root`, no
        // `generation`, and no nulls where a field is simply absent.
        let text = serde_json::to_string(&document).unwrap();
        assert!(!text.contains("schema_version"), "{text}");
        assert!(!text.contains("admin_keys"), "{text}");
        assert!(!text.contains("signed_body_bytes"), "{text}");
        let back: nucleos::wire::api::Deployment = serde_json::from_str(&text).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            text,
            "a round trip through the document changes nothing"
        );
    }

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
