use super::files::subset_or_all;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The network posture.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NetMode {
    /// Leaves networking to the host: not a posture, the absence of one.
    #[default]
    Default,
    /// Traffic goes through `proxy`.
    Proxy,
    /// Only the listed destinations.
    AllowList,
    /// Nothing outbound.
    DenyAll,
}

/// A rule's verdict.
///
/// `Deny` is the zero value and is never omitted from a document: a dropped deny
/// rule would invert the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetAction {
    /// Refuse it.
    #[default]
    Deny,
    /// Allow it.
    Allow,
}

/// One ordered network rule. The first match wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NetRule {
    /// The verdict.
    #[serde(default)]
    pub action: NetAction,
    /// A hostname suffix or an exact IP literal.
    pub host: String,
    /// One port; absent matches any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Attached to a deny rule so the model can read what was blocked and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// TLS interception for `CONNECT` traffic.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Mitm {
    /// Whether interception is on.
    #[serde(default)]
    pub enabled: bool,
    /// Read bodies, not just headers.
    #[serde(default)]
    pub inspect_bodies: bool,
    /// Cap on an inspected body; absent means the backend's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<u64>,
    /// Destinations to intercept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Destinations never to intercept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_hosts: Vec<String>,
}

/// The outbound posture and its allow-lists.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct NetPolicy {
    /// What the backend enforces.
    #[serde(default)]
    pub mode: NetMode,
    /// Hostname suffixes and exact IPs, optionally `:port`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
    /// Ordered rules, first match wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<NetRule>,
    /// The URL traffic is routed through when the mode is [`NetMode::Proxy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Local sockets a sandboxed process may reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unix_sockets: Vec<String>,
    /// TLS interception.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm: Option<Mitm>,
}

impl NetPolicy {
    /// Whether `next` grants no more egress than `self` does.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        // A stricter posture is a higher rank; `Default` is the widest of all.
        next.mode >= self.mode
            && subset_or_all(&self.allow_hosts, &next.allow_hosts)
            && subset_or_all(&self.unix_sockets, &next.unix_sockets)
            && match (&self.mitm, &next.mitm) {
                // Turning interception on narrows; turning it off widens.
                (None, _) => true,
                (Some(_), None) => false,
                (Some(before), Some(after)) => !before.enabled || after.enabled,
            }
    }
}
