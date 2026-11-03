//! One signed request, for every remote command.
//!
//! The three pieces a command needs — where the daemon is, which key signs, and the private half —
//! resolved once, here, so that `nucleos config` and `nucleos models add` cannot disagree about where
//! a client looks. The order of preference is the same for both:

use crate::client::{self, Response};
use crate::config::ClientConfig;
use nucleos::error::{Error, Result};
use nucleos_auth::{KeyPair, KeyStore};
use std::path::PathBuf;

/// What the operator overrode on the command line.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Where the daemon listens.
    pub socket: Option<PathBuf>,
    /// Which key signs.
    pub key_id: Option<String>,
    /// A file holding the private key, for a machine without a keychain.
    pub key_file: Option<PathBuf>,
    /// The client config written by `init`.
    pub client_config: Option<PathBuf>,
}

/// A client that can sign.
pub struct Remote {
    socket: PathBuf,
    key_id: String,
    key: KeyPair,
}

impl Remote {
    /// Resolve the socket and key from the overrides, then the client config, then a clear error.
    pub fn open(overrides: &Overrides) -> Result<Self> {
        let config = match &overrides.client_config {
            Some(path) => ClientConfig::load(path)?,
            None => match crate::init::default_client_config() {
                Some(path) => ClientConfig::load(&path)?,
                None => ClientConfig::default(),
            },
        };
        let socket = overrides.socket.clone().or(config.socket).ok_or_else(|| {
            Error::invalid(
                "no socket: pass --socket, or run `nucleos init` so a client config exists",
            )
        })?;
        let key_id = overrides.key_id.clone().or(config.key).ok_or_else(|| {
            Error::invalid(
                "no key: pass --key-id, or run `nucleos init` so a client config names one",
            )
        })?;
        // The private key never lives beside the socket path: it comes from the keychain, or from an
        // explicitly named file for a machine that has no keychain at all.
        let store = match &overrides.key_file {
            Some(path) => KeyStore::File(path.clone()),
            None => KeyStore::keychain(key_id.clone()),
        };
        let key = KeyPair::from_secret(&store.get()?)?;
        Ok(Self {
            socket,
            key_id,
            key,
        })
    }

    /// Sign one request and send it.
    pub fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<Response> {
        self.request_with(method, path, body, &[])
    }

    /// The same request, with headers beyond the credential.
    pub fn request_with(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<Response> {
        let authorization =
            self.key
                .sign_request(&self.key_id, method, path, body, now_unix_millis());
        client::request(&self.socket, method, path, body, &authorization, extra)
    }

    /// Sign, send, and insist on a 2xx — with the daemon's own words when it refuses.
    pub fn call(&self, method: &str, path: &str, body: &[u8]) -> Result<String> {
        self.call_with(method, path, body, &[])
    }

    /// The same request, with headers the signature does not cover.
    ///
    /// `If-Match` is the one that matters: without it an apply is a last-writer-wins overwrite, and the
    /// CLI would be the tool that makes the dangerous version the convenient one.
    pub fn call_with(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<String> {
        let response = self.request_with(method, path, body, extra)?;
        if !(200..300).contains(&response.status) {
            // The daemon answers failures as `{code, reason}`; carrying that through is the difference
            // between `Error: this deployment is at revision 1, ...` and a debug dump of a struct. The
            // code matters too — `conflict` tells the caller to re-read, `invalid` to fix the request.
            if let Ok(body) =
                serde_json::from_str::<nucleos::wire::api::ErrorBody>(response.body.trim())
            {
                return Err(Error::new(body.code, body.reason));
            }
            return Err(Error::new(
                nucleos::contract::ErrorCode::Denied,
                format!(
                    "the daemon refused the request ({}): {}",
                    response.status,
                    response.body.trim()
                ),
            ));
        }
        Ok(response.body.trim().to_owned())
    }
}

/// Milliseconds since the epoch.
#[must_use]
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
