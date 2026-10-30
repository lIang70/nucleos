//! The nucleos daemon.
//!
//! Three jobs, and the boundary is the point:
//!
//! - **Authentication.** Turn a connection into a `Principal{tenant, subject}`. The kernel never
//!   verifies a signature; it receives the principal as data and hands it to policy.
//! - **Translation.** Convert wire types into kernel types and events back. Every value that
//!   exists only because the wire needs it stops here.
//! - **Transport.** The listener, TLS material, and connection limits. None of these are
//!   hot-reloadable — see `docs/pitfalls.md` §12.
//!
//! The kernel itself is a library that knows none of the above.
//!
//! Today the daemon serves `Describe`, `Start` and `Run` (replay), plus a HITL verdict through
//! `Send`. Everything else returns an explicit `UNIMPLEMENTED` with the reason, because a
//! method that pretends to work is worse than one that says what it is waiting for.

use nucleosd::{App, router};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

/// A unix socket by default, in a directory only its owner can enter.
///
/// The socket's permissions *are* the authentication this daemon can honestly claim: peer
/// credentials would need `unsafe`, which this workspace forbids, so the check is pushed to the one
/// place that can enforce it — the filesystem. `NUCLEOS_TOKEN` adds a bearer token on top for a
/// deployment that exposes the socket beyond its own user.
const DEFAULT_SOCKET: &str = "/tmp/nucleosd.sock";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_owned());

    // A private directory first: `0600` on a socket inside a world-writable directory does not
    // stop anyone from connecting, because entering the directory is not what they need to do.
    if let Some(parent) = std::path::Path::new(&socket).parent() {
        std::fs::create_dir_all(parent)?;
        // Best effort and deliberate: a socket in `/tmp` cannot be made private by chmod-ing
        // `/tmp`, and refusing to start would make the default unusable. The warning says what the
        // deployment actually has.
        if std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).is_err() {
            eprintln!(
                "warning: could not restrict {} to its owner; the socket's permissions are the \
                 authentication, so point nucleosd at a private directory or set NUCLEOS_TOKEN",
                parent.display()
            );
        }
    }

    // A stale socket file from a previous run would make `bind` fail with an address-in-use
    // error that names nothing.
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
    }

    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

    // A deployment file is the next slice; until then the environment is the configuration, so a
    // real model can be served without one.
    let app = App::from_env().await?;

    let token = std::env::var("NUCLEOS_TOKEN").is_ok_and(|token| !token.is_empty());
    eprintln!(
        "nucleosd listening on unix:{socket} (authentication: {})",
        if token {
            "bearer token + socket permissions"
        } else {
            "socket permissions only — set NUCLEOS_TOKEN to require a token"
        }
    );
    axum::serve(listener, router(Arc::new(app)))
        .await
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

    Ok(())
}
