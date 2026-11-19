//! What a command may touch, and the words the kernel confines it with.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a session's commands may touch.
///
/// Set when the session opens and carried in its checkpoint. It is **not** a field of a request:
/// a caller that can name its own policy can name `DangerFullAccess`, and a permission slip the
/// untrusted side writes is not a permission model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// No restriction. Use with caution, and only where the code is yours.
    DangerFullAccess,
    /// Read everything, write nothing.
    ReadOnly {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// Write inside the session's workspace and nothing else.
    WorkspaceWrite {
        /// Extra roots outside the workspace that may be written.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_writable_roots: Vec<String>,
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// The process is already outside our reach; only the network setting is ours to honour.
    ExternalSandbox {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
}

/// The default is the useful one, not the loosest: write inside the workspace, no network.
impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        }
    }
}

impl SandboxPolicy {
    /// The name a policy rule matches, and the name an error message uses.
    ///
    /// A string rather than a `Debug` rendering: a rule reads `{"sandbox": "workspace_write"}`, and a
    /// rename in the enum has to be a decision about the wire, not a side effect of refactoring.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DangerFullAccess => "danger_full_access",
            Self::ReadOnly { .. } => "read_only",
            Self::WorkspaceWrite { .. } => "workspace_write",
            Self::ExternalSandbox { .. } => "external_sandbox",
        }
    }

    /// Every root this policy would let a command write to.
    ///
    /// Both the session's workspace and the extra roots a caller may add: bounding one and not the
    /// other would leave the widening reachable through the field nobody bounded.
    #[must_use]
    pub fn writable_roots<'a>(&'a self, workspace: Option<&'a str>) -> Vec<&'a str> {
        let mut roots = Vec::new();
        match self {
            Self::DangerFullAccess | Self::ReadOnly { .. } => {}
            Self::WorkspaceWrite {
                extra_writable_roots,
                ..
            } => {
                if let Some(workspace) = workspace {
                    roots.push(workspace);
                }
                roots.extend(extra_writable_roots.iter().map(String::as_str));
            }
            // Outside the kernel's reach: the policy is somebody else's, so nothing here bounds it.
            Self::ExternalSandbox { .. } => {}
        }
        roots
    }

    /// The narrowest policy: read only, no network.
    #[must_use]
    pub fn read_only() -> Self {
        Self::ReadOnly {
            network_access: false,
        }
    }

    /// Whether outbound network is permitted under this policy.
    #[must_use]
    pub fn network_access(&self) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly { network_access }
            | Self::WorkspaceWrite { network_access, .. }
            | Self::ExternalSandbox { network_access } => *network_access,
        }
    }

    /// Whether moving from this policy to `next` only ever takes room away.
    ///
    /// The same rule tools follow — narrowing is a configuration change, widening is not — applied
    /// to the one thing a session is allowed to change about its own confinement. A session that
    /// wants *more* access does not reconfigure itself; it asks for an escalation on one command,
    /// which the gate answers.
    ///
    /// The order is by what a process may touch, not by name: `read_only` is narrower than
    /// `workspace_write` because it grants no writes at all, and `danger_full_access` is wider than
    /// both. `external_sandbox` is not a rung on that ladder — it means the kernel is not enforcing
    /// anything — so entering it from a policy the kernel *does* enforce is a widening, and leaving
    /// it is not.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        match (self, next) {
            // Widening, in any of its forms.
            (_, Self::DangerFullAccess) => false,
            (Self::ReadOnly { .. } | Self::WorkspaceWrite { .. }, Self::ExternalSandbox { .. }) => {
                false
            }

            // From full access, or from outside our reach, anything the kernel enforces is
            // narrower.
            (Self::DangerFullAccess, _) | (Self::ExternalSandbox { .. }, _) => true,

            // Within one rung: fewer roots, and no network that was not already allowed.
            (
                Self::ReadOnly {
                    network_access: was,
                },
                Self::ReadOnly { network_access },
            ) => !network_access || *was,
            (Self::ReadOnly { .. }, _) => false,

            // A read-only policy grants no writes, so it is narrower from any writing policy.
            (
                Self::WorkspaceWrite {
                    network_access: was,
                    ..
                },
                Self::ReadOnly { network_access },
            ) => !network_access || *was,
            (
                Self::WorkspaceWrite {
                    extra_writable_roots: was,
                    network_access: was_net,
                },
                Self::WorkspaceWrite {
                    extra_writable_roots,
                    network_access,
                },
            ) => {
                // Compared as written: a root that only differs by a symlink is a different grant,
                // and treating it as equal would let a session swap one directory for another.
                (!network_access || *was_net)
                    && extra_writable_roots.iter().all(|root| was.contains(root))
            }
        }
    }
}

/// Extra room asked for by one command.
///
/// Only ever widens: a caller cannot use it to narrow the session's policy, because narrowing by
/// request would let one command quietly change what a later command is allowed to assume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditionalPermissions {
    /// Roots outside the workspace this command may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
    /// Whether this command may reach the network.
    #[serde(default)]
    pub network_access: bool,
}

/// What a single `exec` asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPermissions {
    /// Run under the session's policy.
    #[default]
    UseDefault,
    /// Run outside the sandbox. The gate turns this into a human question, not a formality.
    RequireEscalated,
    /// Stay in the sandbox, with more room for this command only.
    WithAdditionalPermissions {
        /// What to add. Never what to remove.
        additional: AdditionalPermissions,
    },
}

/// Which stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A piece of captured output, and whether it was cut.
///
/// Truncation is carried rather than applied silently: a model that cannot see that its output
/// was shortened will reason about a file it only half read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputChunk {
    /// The text kept.
    pub text: String,
    /// The line after which the rest was dropped, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_after_lines: Option<u32>,
}

/// How one command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecOutput {
    /// The exit status, or `-1` when the process was killed by a signal.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: OutputChunk,
    /// Standard error.
    pub stderr: OutputChunk,
    /// How long it ran.
    pub duration_millis: u64,
    /// Whether it was killed for exceeding its deadline.
    pub timed_out: bool,
}
