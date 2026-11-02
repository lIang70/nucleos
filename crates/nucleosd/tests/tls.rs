//! The TCP transport, and the refusal that comes with it.
//!
//! Two claims, and the second is the one that matters. TLS works — a real handshake, a real request,
//! a real answer — and plaintext TCP is refused at start-up rather than warned about in a log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Principal, Routing};
use nucleos::kernel::Kernel;
use nucleosd::{App, router, serve};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture() -> Arc<App> {
    let kernel = Kernel::builder()
        .model(
            "primary",
            Arc::new(nucleos_testkit::FakeModel::text(["unused"])),
        )
        .store(Arc::new(nucleos_sqlite::SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    Arc::new(App::new(
        Arc::new(kernel),
        Principal::default(),
        vec!["primary".to_owned()],
    ))
}

/// A self-signed certificate for `localhost`, on disk where the daemon wants it.
fn certificate() -> (serve::Tls, rustls::pki_types::CertificateDer<'static>) {
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    // Unique per call: the tests in this binary run in parallel, and two of them writing one
    // certificate directory would have one test trust a certificate the other replaced.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!("nucleos-tls-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let cert = directory.join("cert.pem");
    let key = directory.join("key.pem");
    std::fs::write(&cert, issued.cert.pem()).unwrap();
    std::fs::write(&key, issued.key_pair.serialize_pem()).unwrap();
    (
        serve::Tls { cert, key },
        issued.cert.der().clone(),
    )
}

fn client(trusting: rustls::pki_types::CertificateDer<'static>) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trusting).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn describe_answers_over_tls() {
    let (tls, trusted) = certificate();
    let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = bound.local_addr().unwrap();
    let server = tokio::spawn(serve::serve_tls(
        bound,
        tls.clone(),
        router(fixture()),
        // Nothing asks it to stop; the test drops the task.
        std::future::pending(),
    ));

    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut stream = client(trusted).connect(name, stream).await.unwrap();

    stream
        .write_all(b"GET /v1/describe HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    // The answer is the daemon's, not a proxy's: the schema id is the ABI the router was built for.
    assert!(response.contains("\"abi_version\""), "{response}");
    server.abort();
}

#[tokio::test]
async fn a_caller_that_does_not_trust_the_certificate_is_refused() {
    let (tls, _trusted) = certificate();
    let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = bound.local_addr().unwrap();
    let server = tokio::spawn(serve::serve_tls(
        bound,
        tls.clone(),
        router(fixture()),
        std::future::pending(),
    ));

    // A client with the system roots only: the certificate is self-signed and unknown, so the
    // handshake must fail. This is the half of TLS that is easy to lose — a listener that negotiates
    // with anyone is not encrypting anything in particular.
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    assert!(
        connector.connect(name, stream).await.is_err(),
        "an unknown certificate is not a certificate"
    );
    server.abort();
}

#[test]
fn a_tcp_listener_without_tls_is_refused_before_anything_is_bound() {
    let directory = std::env::temp_dir().join(format!("nucleos-tls-flags-{}", std::process::id()));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nucleosd"))
        .arg("--root")
        .arg(&directory)
        .arg("--tcp")
        .arg("127.0.0.1:0")
        .output()
        .unwrap();
    assert!(!output.status.success(), "the daemon started without TLS");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("plaintext"), "{stderr}");
    assert!(stderr.contains("--tls-cert"), "{stderr}");
}

#[test]
fn tls_material_without_a_listener_is_refused() {
    // The mirror image: a certificate nobody serves is a deployment that believes it is encrypted.
    let directory = std::env::temp_dir().join(format!("nucleos-tls-orphan-{}", std::process::id()));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nucleosd"))
        .arg("--root")
        .arg(&directory)
        .arg("--tls-cert")
        .arg("/etc/nucleos/cert.pem")
        .arg("--tls-key")
        .arg("/etc/nucleos/key.pem")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--tcp"), "{stderr}");
}
