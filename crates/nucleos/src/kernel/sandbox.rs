//! The mechanism behind [`SandboxPolicy`].
//!
//! A policy is a claim about what a command may touch. Until this module existed, the kernel could
//! not keep that claim: anything stricter than `danger_full_access` was refused, which is honest but
//! useless — a contract that advertises four levels and implements one is a document, not a kernel.
//!
//! On macOS the mechanism is **seatbelt**: `sandbox-exec` with a profile written by this module. The
//! profile is deny-by-default and allows exactly what the policy names, in this order:
//!
//! - read everything (no policy in this ABI restricts reading, and pretending otherwise would break
//!   every program's attempt to load a shared library),
//! - spawn processes and signal itself, because a command that cannot fork is not a command,
//! - write to the standard devices, because a program that cannot write to `/dev/null` fails at
//!   start-up for a reason nobody can act on,
//! - write under the roots the policy names, and nothing else,
//! - reach the network, only when the policy says so.
//!
//! Two deliberate refusals:
//!
//! - **No implicit temporary directory.** `workspace_write` means the workspace. A tool that needs
//!   `TMPDIR` says so in `extra_writable_roots`; a sandbox that quietly widens to make one program
//!   comfortable is a sandbox whose edge nobody can predict.
//! - **Nothing on other platforms yet.** Linux needs bubblewrap and landlock, which are not written;
//!   a Linux deployment gets `Unavailable` rather than a promise it cannot keep.
//!
//! `sandbox-exec` is deprecated in the sense Apple stops mentioning it, not in the sense it stops
//! working: it is the only unprivileged mechanism macOS offers, and the alternative is refusing
//! every command on the platform people develop on.

use crate::contract::{SandboxPermissions, SandboxPolicy};
use crate::error::Result;

/// How to run one command under a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confinement {
    /// The program to execute in place of the command.
    pub program: String,
    /// Arguments that go before the command's own.
    pub prefix: Vec<String>,
    /// What a human should read: which mechanism, and for which policy.
    pub mechanism: &'static str,
    /// The profile the mechanism was given, when it takes one.
    ///
    /// Kept so the log can answer "why was this refused" without reconstructing the policy — which
    /// may have changed, and would be a second implementation to disagree with this one.
    pub profile: Option<String>,
}

/// Wrap a command, or refuse because this platform has no mechanism for the policy asked for.
///
/// `None` means "run it as it is": it is the honest answer for `danger_full_access`, for a sandbox
/// the caller operates itself, and for an escalation the gate has already approved.
pub(crate) fn confine(
    policy: &SandboxPolicy,
    permissions: &SandboxPermissions,
    workspace: Option<&String>,
) -> Result<Option<Confinement>> {
    match policy {
        // Nothing to enforce, and saying so is the whole point of the name.
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => Ok(None),
        // An escalation is the gate's decision, not the sandbox's: policy allowed it, so it runs
        // outside. The one protection left is the gate, which is why the contract says the gate
        // turns an escalation into a question rather than a formality.
        SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. }
            if matches!(permissions, SandboxPermissions::RequireEscalated) =>
        {
            Ok(None)
        }
        SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. } => {
            seatbelt(policy, permissions, workspace)
        }
    }
}

#[cfg(target_os = "macos")]
fn seatbelt(
    policy: &SandboxPolicy,
    permissions: &SandboxPermissions,
    workspace: Option<&String>,
) -> Result<Option<Confinement>> {
    let profile = profile(policy, permissions, workspace);
    Ok(Some(Confinement {
        program: "/usr/bin/sandbox-exec".to_owned(),
        prefix: vec!["-p".to_owned(), profile.clone()],
        mechanism: "seatbelt",
        profile: Some(profile),
    }))
}

#[cfg(not(target_os = "macos"))]
fn seatbelt(
    _policy: &SandboxPolicy,
    _permissions: &SandboxPermissions,
    _workspace: Option<&String>,
) -> Result<Option<Confinement>> {
    Err(crate::error::Error::unavailable(
        "no sandbox mechanism is available on this platform: macOS has seatbelt, and the \
         bubblewrap and landlock path for Linux is not written yet. Only `danger_full_access` and \
         the external sandbox can run here",
    ))
}

/// The writable roots and network setting a policy and one command's permissions add up to.
fn writable(policy: &SandboxPolicy, permissions: &SandboxPermissions, workspace: Option<&String>) -> (bool, Vec<String>) {
    let mut network = policy.network_access();
    let mut roots = Vec::new();

    if let SandboxPolicy::WorkspaceWrite {
        extra_writable_roots,
        ..
    } = policy
    {
        // A workspace policy with no workspace has no writable root, which is the correct reading
        // rather than a configuration to guess at.
        roots.extend(workspace.cloned());
        roots.extend(extra_writable_roots.iter().cloned());
    }

    if let SandboxPermissions::WithAdditionalPermissions { additional } = permissions {
        network = network || additional.network_access;
        roots.extend(additional.writable_roots.iter().cloned());
    }

    (network, roots)
}

/// The seatbelt profile for a policy.
///
/// Written as text with one rule per line so that a refusal can be read: when a command fails with
/// "Operation not permitted", the profile is the document that says why.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn profile(
    policy: &SandboxPolicy,
    permissions: &SandboxPermissions,
    workspace: Option<&String>,
) -> String {
    let (network, roots) = writable(policy, permissions, workspace);
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         ; Reading is not restricted by any policy in this ABI: every program loads a library\n\
         ; before it does anything, and a sandbox that breaks that is a sandbox nobody keeps.\n\
         (allow file-read*)\n\
         (allow process*)\n\
         (allow signal (target self))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow ipc-posix-shm)\n\
         ; A program that cannot write to its own standard devices fails at start-up, for a reason\n\
         ; that has nothing to do with the command.\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\") \
         (literal \"/dev/tty\") (literal \"/dev/dtracehelper\"))\n",
    );

    // Namespaces under /dev that a program opens by descriptor rather than by name.
    profile.push_str("(allow file-write* (subpath \"/dev/fd\"))\n");

    for root in &roots {
        // Both the path as given and the path as the kernel sees it. On macOS `/var` is a symlink
        // to `/private/var`, and seatbelt matches against the resolved path — so a profile that
        // names only what the caller typed denies writes to the very directory it meant to allow.
        for real in realized(root) {
            profile.push_str(&format!("(allow file-write* (subpath {}))\n", quoted(&real)));
        }
    }
    if network {
        profile.push_str("(allow network*)\n");
    }
    profile
}

/// A path, and the path it resolves to when that is different.
///
/// A root that does not exist yet is kept as written: a caller may be granting a directory the
/// command itself is about to create, and refusing to name it would be refusing a legitimate grant.
fn realized(path: &str) -> Vec<String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) if resolved != std::path::Path::new(path) => {
            vec![path.to_owned(), resolved.to_string_lossy().into_owned()]
        }
        _ => vec![path.to_owned()],
    }
}

/// A path as a seatbelt string literal.
///
/// Escaped rather than trusted: a workspace under a directory with a quote in its name would
/// otherwise close the literal early and turn the rest of the path into profile syntax.
fn quoted(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len() + 2);
    escaped.push('"');
    for character in path.chars() {
        match character {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            // A control character inside a literal is legal to a parser and invisible to a reader,
            // and a newline makes a profile look like it has a rule it does not have. Escaped, it
            // stays one string.
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::contract::AdditionalPermissions;

    fn workspace() -> String {
        "/srv/workspace".to_owned()
    }

    #[test]
    fn full_access_and_an_external_sandbox_are_not_wrapped() {
        assert!(
            confine(&SandboxPolicy::DangerFullAccess, &SandboxPermissions::UseDefault, None)
                .unwrap()
                .is_none()
        );
        assert!(
            confine(
                &SandboxPolicy::ExternalSandbox {
                    network_access: true
                },
                &SandboxPermissions::UseDefault,
                None
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn an_approved_escalation_runs_outside_the_sandbox() {
        // The gate allowed it; wrapping it anyway would make the escalation mean nothing.
        assert!(
            confine(
                &SandboxPolicy::read_only(),
                &SandboxPermissions::RequireEscalated,
                Some(&workspace())
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn the_profile_denies_by_default_and_names_what_it_allows() {
        let profile = profile(
            &SandboxPolicy::WorkspaceWrite {
                extra_writable_roots: Vec::new(),
                network_access: false,
            },
            &SandboxPermissions::UseDefault,
            Some(&workspace()),
        );

        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-write* (subpath \"/srv/workspace\"))"));
        assert!(
            !profile.contains("(allow network*)"),
            "the default policy has no network:\n{profile}"
        );
    }

    #[test]
    fn a_read_only_policy_names_no_writable_root() {
        let profile = profile(
            &SandboxPolicy::read_only(),
            &SandboxPermissions::UseDefault,
            Some(&workspace()),
        );
        assert!(
            !profile.contains("(subpath \"/srv/workspace\")"),
            "read-only must not inherit a workspace root:\n{profile}"
        );
    }

    #[test]
    fn permissions_widen_and_never_narrow() {
        let policy = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: vec!["/srv/cache".to_owned()],
            network_access: false,
        };
        let profile = profile(
            &policy,
            &SandboxPermissions::WithAdditionalPermissions {
                additional: AdditionalPermissions {
                    writable_roots: vec!["/srv/extra".to_owned()],
                    network_access: true,
                },
            },
            Some(&workspace()),
        );

        // The policy's own roots survive; the command's are added; the network only ever turns on.
        assert!(profile.contains("/srv/workspace"));
        assert!(profile.contains("/srv/cache"));
        assert!(profile.contains("/srv/extra"));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn a_symlinked_root_is_named_both_ways() {
        // The failure this prevents is the one the end-to-end test caught first: on macOS the temp
        // directory is reached through `/var`, which is a symlink to `/private/var`, and a profile
        // naming only the former denies the writes it was written to allow.
        let directory = std::env::temp_dir().join(format!("nucleos-realized-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let profile = profile(
            &SandboxPolicy::WorkspaceWrite {
                extra_writable_roots: Vec::new(),
                network_access: false,
            },
            &SandboxPermissions::UseDefault,
            Some(&directory.to_string_lossy().into_owned()),
        );

        let canonical = std::fs::canonicalize(&directory).unwrap();
        assert!(
            profile.contains(&quoted(&canonical.to_string_lossy())),
            "the resolved path must be in the profile:\n{profile}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_path_cannot_close_the_literal_it_is_written_into() {
        let profile = profile(
            &SandboxPolicy::WorkspaceWrite {
                extra_writable_roots: vec!["/srv/tricky\"(allow network*)".to_owned()],
                network_access: false,
            },
            &SandboxPermissions::UseDefault,
            None,
        );

        // The quote is escaped, so the injected text stays inside the literal — and a reader
        // scanning the profile line by line sees no rule that grants the network.
        assert!(
            profile.contains(r#"/srv/tricky\"(allow network*)"#),
            "{profile}"
        );
        assert!(
            !profile
                .lines()
                .any(|line| line.trim_start().starts_with("(allow network*)")),
            "the injection became a rule:\n{profile}"
        );
    }
}
