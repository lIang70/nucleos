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
use std::sync::Arc;
/// A unix socket by default: the peer is already the local user, which is the only
/// authentication this daemon can honestly claim until per-connection credentials exist.
const DEFAULT_SOCKET: &str = "/tmp/nucleosd.sock";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_owned());

    // A stale socket file from a previous run would make `bind` fail with an address-in-use
    // error that names nothing.
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
    }

    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

    let app = match std::env::var("NUCLEOS_DEPLOYMENT") {
        // A deployment file is the next slice; until then the daemon is up, describes itself, and
        // refuses to pretend it has models.
        Ok(path) => {
            eprintln!(
                "nucleosd: deployment files are not implemented yet ({path}); serving /v1/describe"
            );
            App::unconfigured()
        }
        Err(_) => App::unconfigured(),
    };

    eprintln!("nucleosd listening on unix:{socket}");
    axum::serve(listener, router(Arc::new(app)))
        .await
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

    Ok(())
}
