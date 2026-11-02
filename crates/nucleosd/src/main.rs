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

use nucleosd::{App, Layout, router, serve, telemetry};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Before anything else: a start-up failure has to be readable, and a subscriber installed after
    // the first mistake is a subscriber that missed it.
    let parsed = args(&std::env::args().skip(1).collect::<Vec<_>>())?;
    telemetry::install(&parsed.log);
    let layout = match parsed.mode {
        Mode::Paths(layout) => {
            println!("{}", layout.describe());
            return Ok(());
        }
        Mode::Run(layout) => layout,
    };

    // Directories first, with the modes the socket's permissions depend on. `Layout::create` narrows
    // anything too open rather than trusting that an installer did it.
    layout
        .create()
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
    // Before the socket is bound and before a store is opened: two writers on one root interleave
    // sequence numbers into a log that looks fine until a `load` refuses it, and nothing downstream
    // would ever attribute that to a second daemon.
    let _holding_the_root = nucleosd::layout::claim(&layout)?;
    let socket = layout.socket.clone();

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
    let app = App::load(&layout).await?;

    tracing::info!(
        event = "daemon_started",
        root = %layout.root.display(),
        socket = %socket.display(),
        version = env!("CARGO_PKG_VERSION"),
        abi = nucleosd::http::ABI_VERSION,
    );

    // One signal, two listeners: whichever is configured stops at the same moment, or a restart would
    // leave one of them accepting connections into a kernel that is closing.
    let (stop, watching) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop.send(true);
    });

    let app = Arc::new(app);
    let served = match &parsed.tls {
        None => {
            axum::serve(listener, router(Arc::clone(&app)))
                .with_graceful_shutdown(drain(watching))
                .await
        }
        Some((address, tls)) => {
            // Bound before it is served, so a port already in use is reported as an address rather than
            // as a handshake failure somewhere later.
            let bound = std::net::TcpListener::bind(address).map_err(
                |error| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(std::io::Error::other(format!(
                        "cannot listen on {address}: {error}"
                    )))
                },
            )?;
            tracing::info!(
                event = "tls_listening",
                address = %address,
                cert = %tls.cert.display(),
            );
            // Both, not either: a deployment adds the TCP listener so a remote caller stops needing a
            // socket, and the local one keeps working. `try_join` because a listener that failed to
            // start must not leave the other one serving into a half-started daemon.
            let unix = async {
                axum::serve(listener, router(Arc::clone(&app)))
                    .with_graceful_shutdown(drain(watching.clone()))
                    .await
            };
            let tcp = serve::serve_tls(
                bound,
                tls.clone(),
                router(Arc::clone(&app)),
                drain(watching.clone()),
            );
            tokio::try_join!(unix, tcp).map(|_| ())
        }
    };
    // A stale socket is a start-up failure next time, with an error that names nothing. Removing it
    // on the way out is the difference between a restart and an incident.
    let _ = std::fs::remove_file(&socket);
    tracing::info!(event = "daemon_stopped");
    served.map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;

    Ok(())
}

/// Resolve when the shared stop flag is set.
///
/// A `watch` rather than a `Notify`: two listeners have to observe the same signal, and a notification
/// waits for whoever is looking at the moment it fires.
async fn drain(mut watching: tokio::sync::watch::Receiver<bool>) {
    while !*watching.borrow() {
        if watching.changed().await.is_err() {
            return;
        }
    }
}

/// Wait for a signal that means "stop", so the socket is removed and the stop is recorded.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(event = "signal_handler_unavailable", detail = %error);
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!(event = "shutdown_requested", signal = "int"),
            _ = terminate.recv() => tracing::info!(event = "shutdown_requested", signal = "term"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// What the command line asked for.
#[derive(Debug)]
struct Flags {
    mode: Mode,
    /// The `tracing` filter directive. A flag, because a daemon whose verbosity comes from the
    /// environment logs differently on the machine you are debugging.
    log: String,
    /// The TCP address and its material, when the daemon listens on one.
    tls: Option<(std::net::SocketAddr, serve::Tls)>,
}

/// What to do.
#[derive(Debug)]
enum Mode {
    /// Print the paths and stop.
    Paths(Layout),
    /// Serve.
    Run(Layout),
}

/// The whole command line: `--root`, `--socket`, `--print-paths`.
///
/// Two flags because there are two things a deployment decides: where the data is, and where the
/// socket is. Everything else is derived or is configuration, which arrives later over the ABI rather
/// than on a command line.
fn args(args: &[String]) -> Result<Flags, Box<dyn std::error::Error + Send + Sync>> {
    let (mut root, mut socket, mut seal_key, mut print) = (None, None, None, false);
    let (mut tcp, mut cert, mut key) = (None, None, None);
    let mut log = "info".to_owned();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--root" => {
                root = Some(PathBuf::from(rest.next().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--root needs a path")
                })?));
            }
            "--socket" => {
                socket = Some(PathBuf::from(rest.next().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--socket needs a path")
                })?));
            }
            "--log" => {
                log = rest.next().cloned().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--log needs a level")
                })?;
            }
            "--seal-key" => {
                seal_key = Some(PathBuf::from(rest.next().cloned().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--seal-key needs a path")
                })?));
            }
            "--tcp" => {
                tcp = Some(rest.next().cloned().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--tcp needs an address")
                })?);
            }
            "--tls-cert" => {
                cert = Some(PathBuf::from(rest.next().cloned().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--tls-cert needs a path")
                })?));
            }
            "--tls-key" => {
                key = Some(PathBuf::from(rest.next().cloned().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--tls-key needs a path")
                })?));
            }
            "--print-paths" => print = true,
            other if other.starts_with('-') => {
                return Err(Box::from(format!(
                    "unknown flag {other}; this daemon takes --root <dir>, --socket <path>, \
                     --seal-key <path>, --tcp <addr> --tls-cert <path> --tls-key <path>, \
                     --log <level>, --print-paths"
                )));
            }
            other => {
                return Err(Box::from(format!(
                    "unexpected argument {other:?}; the socket is named with --socket"
                )));
            }
        }
    }

    // All three or none. A TCP listener with no certificate is a bearer token in the clear and a
    // signature handed to whoever is listening; a certificate with no listener is a deployment that
    // believes it is encrypted. Both are refused at start-up, before anything is bound, because a
    // warning in a log is not a gate.
    let tls = match (tcp, cert, key) {
        (None, None, None) => None,
        (Some(addr), Some(cert), Some(key)) => Some((
            addr.parse::<std::net::SocketAddr>().map_err(|error| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "--tcp {addr:?} is not an address: {error}"
                ))
            })?,
            serve::Tls { cert, key },
        )),
        (Some(_), _, _) => {
            return Err(Box::from(
                "--tcp needs --tls-cert and --tls-key: a bearer token over plaintext TCP is a secret \
                 sent in the clear, and this daemon will not do that",
            ));
        }
        _ => {
            return Err(Box::from(
                "--tls-cert and --tls-key are set but --tcp is not: nothing would serve them",
            ));
        }
    };

    let layout = Layout::resolve(root, socket, seal_key);
    Ok(Flags {
        mode: if print {
            Mode::Paths(layout)
        } else {
            Mode::Run(layout)
        },
        log,
        tls,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn two_paths_and_one_switch() {
        let flags = args(&[
            "--root".to_owned(),
            "/srv/a".to_owned(),
            "--socket".to_owned(),
            "/run/a/s.sock".to_owned(),
        ])
        .unwrap();
        let Mode::Run(layout) = flags.mode else {
            panic!("expected a run");
        };
        assert_eq!(layout.root, PathBuf::from("/srv/a"));
        assert_eq!(layout.socket, PathBuf::from("/run/a/s.sock"));

        assert!(matches!(
            args(&["--print-paths".to_owned()]).unwrap().mode,
            Mode::Paths(_)
        ));
        assert_eq!(
            args(&["--log".to_owned(), "debug".to_owned()]).unwrap().log,
            "debug"
        );
        assert_eq!(
            args(&[]).unwrap().log,
            "info",
            "the default is a level, not a guess"
        );
    }

    #[test]
    fn a_positional_argument_is_refused_rather_than_guessed() {
        // The old binary took the socket as the first positional; a silent reinterpretation would be
        // a deployment writing its data to the working directory.
        let error = args(&["/tmp/s.sock".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("--socket"), "{error}");
    }

    #[test]
    fn an_unknown_flag_names_the_ones_that_exist() {
        let error = args(&["--store".to_owned(), "x".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("--root"), "{error}");
    }
}
