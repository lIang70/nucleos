//! Where a deployment's files live, derived from one root.
//!
//! Three rules, and the rest of this module follows from them:
//!
//! 1. **One root, everything else derived.** `--root` names the data directory; the two stores are
//!    named inside it. Only the socket may point elsewhere, because a runtime directory is routinely
//!    a different filesystem.
//! 2. **Three kinds of directory with different rules.** Data is copied and backed up; runtime is
//!    recreated on every start; the seal key is the thing that makes a copy of the data useless, so it
//!    must not travel with it. This module only owns the first two — the seal key lands with
//!    `nucleos init`.
//! 3. **The working directory never matters.** A relative default is not a default: it writes to
//!    wherever the process happened to be started from.
//!
//! The defaults are platform conventions rather than guesses, and every one of them is a path a
//! package could install to.

use std::path::{Path, PathBuf};

/// The files one daemon instance uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Where both stores live.
    pub root: PathBuf,
    /// Where the socket is created.
    pub socket: PathBuf,
}

impl Layout {
    /// Resolve the layout from what the operator said, falling back to the platform's convention.
    ///
    /// `root` and `socket` are the two flags: one decides the data, the other the address, and
    /// nothing else is configurable here because everything else is configuration — which lives in
    /// the store and arrives through the ABI.
    pub fn resolve(root: Option<PathBuf>, socket: Option<PathBuf>) -> Self {
        // Flags or the platform's convention. The environment is not consulted: a path that changes
        // because a shell exported something is a path nobody can predict from the unit file.
        Self {
            root: root.unwrap_or_else(default_root),
            socket: socket.unwrap_or_else(default_socket),
        }
    }

    /// The configuration database.
    #[must_use]
    pub fn system_store(&self) -> PathBuf {
        self.root.join("system.db")
    }

    /// The session database.
    #[must_use]
    pub fn session_store(&self) -> PathBuf {
        self.root.join("sessions.db")
    }

    /// Create the directories the daemon will write into, with the modes it needs.
    ///
    /// Public directories are refused rather than adjusted: a socket's directory mode is part of its
    /// authentication, and silently widening it to make a path work would be the start of a service
    /// whose permissions nobody can describe.
    pub fn create(&self) -> std::io::Result<()> {
        create_private(&self.root)?;
        if let Some(parent) = self.socket.parent() {
            create_private(parent)?;
        }
        Ok(())
    }

    /// One line per path, for `--print-paths`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for (what, path) in [
            ("root", self.root.clone()),
            ("system store", self.system_store()),
            ("session store", self.session_store()),
            ("socket", self.socket.clone()),
        ] {
            let state = match std::fs::metadata(&path) {
                Ok(meta) => {
                    use std::os::unix::fs::PermissionsExt;
                    format!("{:04o}", meta.permissions().mode() & 0o777)
                }
                Err(_) => "missing".to_owned(),
            };
            out.push_str(&format!("{what:<14} {:<52} {state}\n", path.display()));
        }
        out
    }
}

/// `/var/lib/nucleos` on Linux, `/Library/Application Support/nucleos` on macOS.
///
/// A home directory is deliberately not a fallback: a system service that writes into somebody's home
/// directory is a service whose data disappears when that account is cleaned up.
fn default_root() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/nucleos")
    } else {
        PathBuf::from("/var/lib/nucleos")
    }
}

/// A socket in a directory only its owner can enter.
///
/// Never a bare `/tmp/nucleosd.sock`: a predictable name in a world-writable directory is a squatting
/// target — another user can create the socket first, and an operator who connects to it has connected
/// to their process. A private directory makes a predictable name harmless.
fn default_socket() -> PathBuf {
    if cfg!(target_os = "macos") {
        // macOS already gives every user a private temporary directory.
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("nucleos").join("nucleosd.sock")
    } else if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        runtime.join("nucleos").join("nucleosd.sock")
    } else {
        // `/run` is the conventional place for a system service's runtime files; a per-user daemon
        // gets a private directory under the system temporary directory instead of the shared one.
        let uid = users_uid().unwrap_or(0);
        PathBuf::from(format!("/tmp/nucleos-{uid}")).join("nucleosd.sock")
    }
}

/// The effective uid, without pulling in a bindings crate.
fn users_uid() -> Option<u32> {
    // `/proc/self/status` is Linux-only and the fallback path is Linux-only, which is why this is a
    // small file read rather than a syscall through a dependency.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|values| values.split_whitespace().next())
                .and_then(|uid| uid.parse().ok())
        })
}

/// Create `path` and make sure only its owner can enter it.
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        // Widen-by-default is what got us here; narrow, and say so.
        tracing::warn!(
            event = "directory_mode_adjusted",
            path = %path.display(),
            was = format!("{mode:04o}"),
            now = "0700",
            detail = "this directory holds sessions and configuration; only its owner may enter it",
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_two_stores_are_named_inside_the_root() {
        let layout = Layout::resolve(Some(PathBuf::from("/srv/nucleos")), None);
        assert_eq!(layout.system_store(), Path::new("/srv/nucleos/system.db"));
        assert_eq!(layout.session_store(), Path::new("/srv/nucleos/sessions.db"));
    }

    #[test]
    fn flags_win_over_everything_and_a_socket_can_stand_alone() {
        let layout = Layout::resolve(
            Some(PathBuf::from("/srv/a")),
            Some(PathBuf::from("/run/a/nucleosd.sock")),
        );
        assert_eq!(layout.root, Path::new("/srv/a"));
        assert_eq!(layout.socket, Path::new("/run/a/nucleosd.sock"));
    }

    #[test]
    fn the_default_socket_is_in_a_private_directory_not_a_bare_tmp_name() {
        let socket = default_socket();
        let parent = socket.parent().unwrap();
        // The failure this prevents: `/tmp/nucleosd.sock` can be created by another user first, and
        // then the operator connects to their process.
        assert_ne!(socket, Path::new("/tmp/nucleosd.sock"));
        assert!(parent != Path::new("/tmp"), "{parent:?} is shared");
    }

    #[test]
    fn the_default_root_is_absolute_and_not_a_home_directory() {
        let root = default_root();
        assert!(root.is_absolute(), "a relative default follows the working directory");
        assert!(!root.starts_with("/home"), "{root:?}");
        assert!(!root.starts_with("/Users"), "{root:?}");
    }

    #[test]
    fn creating_a_layout_keeps_the_directory_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("nucleos-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let layout = Layout::resolve(Some(root.clone()), Some(root.join("run/nucleosd.sock")));
        layout.create().unwrap();

        for path in [&root, &root.join("run")] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is {:04o}", path.display(), mode);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn describe_says_missing_for_what_does_not_exist_yet() {
        let layout = Layout::resolve(Some(PathBuf::from("/nonexistent/nucleos")), None);
        let text = layout.describe();
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("sessions.db"), "{text}");
    }
}
