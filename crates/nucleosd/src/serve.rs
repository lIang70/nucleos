//! The transports the daemon can listen on.
//!
//! Two, and they are different in kind. A unix socket has file permissions in front of it, so the
//! kernel's authentication is a second gate on a door that is already locked. TCP has nothing in front
//! of it, which makes TLS the *first* gate rather than a nicety: a bearer token on a plaintext
//! connection is a secret sent in the clear, and a signed administration request on one is worse — it
//! hands out the signature itself.
//!
//! So there is no plaintext TCP option, and not as a warning: `--tcp` without a certificate is a
//! start-up refusal (`main.rs::args`). A daemon that listens in the clear because somebody forgot a
//! flag is a daemon that leaks credentials to whoever is on the network, and the failure is silent by
//! construction — nothing in the system notices.

use axum::Router;
use std::path::PathBuf;

/// The TLS material, when the daemon listens on TCP.
#[derive(Debug, Clone)]
pub struct Tls {
    /// The certificate chain, PEM.
    pub cert: PathBuf,
    /// The private key, PEM.
    pub key: PathBuf,
}

/// Serve `router` over TLS on an already-bound listener.
///
/// The listener is passed in rather than an address so a caller can bind `:0`, read the port it was
/// given, and tell the world — which is what a test does, and what a supervisor that reports its port
/// would do.
///
/// The certificate is read once, at start-up. Reloading it means a restart, for the same reason the
/// seal key does (`pitfalls` 12): a transport that changes under a live connection is a connection
/// nobody can reason about.
pub async fn serve_tls(
    listener: std::net::TcpListener,
    tls: Tls,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Two crypto providers are compiled into this binary: `ring`, through the HTTP client that talks to
    // models, and `aws-lc-rs`, through the TLS server. rustls refuses to guess when both are present and
    // panics at the first handshake, which is a daemon that starts, logs that it is listening, and dies
    // on the first caller. The choice is made here instead of being left to feature unification.
    //
    // `aws-lc-rs` because it is the one the server side is built against; a provider chosen by accident
    // is a provider nobody reviewed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // A std listener is blocking, and registering one with the reactor is a panic rather than an error.
    // Done here rather than left to the caller: a listener that reached this function is one that is
    // about to be served, and every caller would have to remember the same line.
    listener.set_nonblocking(true)?;

    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
        .await
        .map_err(|error| {
            // Named, because the failure of a certificate load is otherwise an `io::Error` that says
            // nothing about which of the two files was wrong.
            std::io::Error::other(format!(
                "cannot load TLS material ({} and {}): {error}",
                tls.cert.display(),
                tls.key.display()
            ))
        })?;
    // Graceful shutdown goes through a handle rather than a wrapper in axum-server 0.8: the accept loop
    // is the future, and the handle is what tells it to stop taking connections and drain the ones it
    // has. The alternative — dropping the future — would cut live requests mid-response.
    let handle = axum_server::Handle::new();
    let stopper = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        stopper.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
    });
    axum_server::from_tcp_rustls(listener, config)?
        .handle(handle)
        .serve(router.into_make_service())
        .await
}
