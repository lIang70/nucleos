//! Logs a library emits, captured through a real subscriber.
//!
//! The point of this file is that instrumentation which nothing observes is dead code with a
//! formatting cost. Each test installs a subscriber that writes into a buffer, drives a kernel
//! through the case, and asserts the record is there — with the fields an operator would need to act
//! on it, not just the sentence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store};
use nucleos::contract::{Principal, Routing, SessionId};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

/// A writer that keeps every line, so a test can read them after the fact.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Capture {
    fn lines(&self) -> Vec<serde_json::Value> {
        let buffer = self.0.lock().unwrap();
        String::from_utf8_lossy(&buffer)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// A subscriber that writes into a capture, scoped to this thread.
///
/// Thread-scoped rather than global on purpose: the global slot accepts one subscriber per process,
/// so a second test would silently capture nothing — which is exactly the failure this file exists
/// to prevent, one layer up. The tests therefore use `current_thread`, so the async body stays on
/// the thread that installed the subscriber.
fn capture() -> (Capture, tracing::subscriber::DefaultGuard) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

/// The event name of a record, wherever the JSON formatter puts its fields.
fn event(record: &serde_json::Value) -> Option<&str> {
    record["fields"]["event"].as_str()
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> Kernel {
    Kernel::builder()
        .model("primary", Arc::new(nucleos_testkit::FakeModel::text(["unused"])))
        .tool(Arc::new(nucleos_testkit::FakeTool::new("echo", "hi")))
        .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_budget_refusal_is_logged_with_the_numbers_behind_it() {
    let kernel = kernel();
    let session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await
        .unwrap();
    let mut session = session;

    let (capture, _guard) = capture();
    let refused = kernel.inference(&mut session, &[]).await;
    assert!(refused.is_err());

    let lines = capture.lines();
    let record = lines
        .iter()
        .find(|line| event(line) == Some("budget_refused"))
        .unwrap_or_else(|| panic!("no budget record in {lines:#?}"));

    // The fields are the point: a warning that says "out of budget" without saying which budget,
    // for which session, moves the investigation one step back.
    assert_eq!(record["level"], "WARN");
    let fields = &record["fields"];
    assert_eq!(fields["session"], "s1");
    assert_eq!(fields["stop"], "Tokens");
    assert!(fields["credit"].is_number());
}

#[tokio::test(flavor = "current_thread")]
async fn publishing_a_generation_is_logged_with_its_number() {
    let kernel = kernel();
    let (capture, _guard) = capture();
    kernel
        .publish(vec![Arc::new(nucleos_testkit::FakeTool::new("other", "x"))])
        .unwrap();

    let lines = capture.lines();
    let record = lines
        .iter()
        .find(|line| event(line) == Some("generation_published"))
        .unwrap_or_else(|| panic!("no generation record in {lines:#?}"));
    assert_eq!(record["level"], "INFO");
    assert!(record["fields"]["generation"].as_u64().unwrap() >= 2);
    assert!(record["fields"]["tools"].as_u64().unwrap() >= 1);
}
