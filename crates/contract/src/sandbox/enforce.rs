use super::*;
use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which policy dimensions a backend can actually enforce.
///
/// A conservative `false` means **not enforced**, never "unknown": callers and
/// tools never discover features by trial. An unsupported operation is an
/// [`crate::ErrorKind::Unsupported`] error, and this declaration says up front why.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Enforcement {
    /// The environment allow-list is honoured.
    #[serde(default)]
    pub env_allow_list: bool,
    /// The network postures this backend can enforce at the OS level.
    /// `Default` is never listed: it is the absence of a posture, not one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net_modes: Vec<NetMode>,
    /// The host-side proxy can dial a `socks5://` upstream.
    #[serde(default)]
    pub socks5: bool,
    /// TLS can be terminated for `CONNECT` traffic.
    #[serde(default)]
    pub mitm: bool,
    /// Unix socket egress can be confined to an allow-list.
    #[serde(default)]
    pub unix_socket_policy: bool,
    /// The memory ceiling is enforced rather than rejected.
    #[serde(default)]
    pub memory_cap: bool,
    /// The CPU ceiling is enforced.
    #[serde(default)]
    pub cpu_cap: bool,
    /// The disk ceiling is enforced.
    #[serde(default)]
    pub disk_cap: bool,
    /// Writes are confined to the write set at the OS level (a profile, namespace
    /// mounts). A backend without an OS boundary reports `false`, and then a
    /// confine that restricts writes is refused at construction: checking a
    /// workdir at call time is validation, not enforcement.
    #[serde(default)]
    pub filesystem_bounds: bool,
}

impl Enforcement {
    /// The construction-time gate: can this backend honour what it was built for?
    ///
    /// Called once, before anything is opened, so a posture the backend cannot
    /// keep is an error where an operator sees it rather than a failure on the
    /// first command that needs it.
    pub fn can_enforce(&self, confine: &Confine) -> Result<()> {
        let unsupported = |what: &str| {
            Err(Error::new(
                crate::ErrorKind::Unsupported,
                format!("this sandbox backend cannot enforce {what}"),
            ))
        };
        let bounds_writes = confine.files.read_only || !confine.files.write.is_empty();
        if bounds_writes && !self.filesystem_bounds {
            return unsupported("a file write boundary");
        }
        if confine.net.mode != NetMode::Default && !self.net_modes.contains(&confine.net.mode) {
            return unsupported(&format!("network mode {:?}", confine.net.mode));
        }
        if confine.net.mitm.as_ref().is_some_and(|mitm| mitm.enabled) && !self.mitm {
            return unsupported("TLS interception");
        }
        if !confine.net.unix_sockets.is_empty() && !self.unix_socket_policy {
            return unsupported("unix socket confinement");
        }
        if !confine.env.allow.is_empty() && !self.env_allow_list {
            return unsupported("an environment allow-list");
        }
        if confine.resources.memory_bytes.is_some() && !self.memory_cap {
            return unsupported("a memory ceiling");
        }
        if confine.resources.cpu_millicores > 0 && !self.cpu_cap {
            return unsupported("a CPU ceiling");
        }
        if confine.resources.disk_bytes.is_some() && !self.disk_cap {
            return unsupported("a disk ceiling");
        }
        Ok(())
    }
}

/// Which session features a backend provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionFeatures {
    /// Pty sessions: resize, and stdout merged with stderr.
    #[serde(default)]
    pub tty: bool,
    /// Signal delivery.
    #[serde(default)]
    pub signal: bool,
    /// Push event streams.
    #[serde(default)]
    pub events: bool,
}

/// A backend's whole declaration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendCapabilities {
    /// What it can enforce.
    #[serde(default)]
    pub enforcement: Enforcement,
    /// What session features it provides.
    #[serde(default)]
    pub features: SessionFeatures,
}

// ---------------------------------------------------------------------------
// One command
// ---------------------------------------------------------------------------
