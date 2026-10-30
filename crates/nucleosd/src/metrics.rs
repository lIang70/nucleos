//! What this daemon has done, counted.
//!
//! Telemetry is usually three products — metrics, logs, traces — and this is deliberately the first
//! slice of one: counters a person can read while the daemon is up, and one JSON line per request on
//! stdout. What is absent and why:
//!
//! - **No histograms.** A p99 of request duration is only useful with a sink that can aggregate it,
//!   and a daemon exporting to nowhere is a promise about dashboards nobody has. The line per request
//!   carries the duration; a collector can build its own histogram from it.
//! - **No trace context.** The kernel's event log is already a better trace than an opaque span: it
//!   has sequence numbers, it is durable, and it is replayable. A second trace would be a second
//!   ordering of the same facts.
//! - **No exporter.** Files and sockets are deployment decisions, and there is no deployment file
//!   yet.
//!
//! The counters are deliberately few and named after questions an operator asks: how many requests,
//! which route, how many refused, how many sessions started, how many stopped on budget, how many
//! reloads. Anything that is not one of those questions can be computed from the event log, which is
//! the record rather than a statistic.

use nucleos::wire::api::MetricsResponse;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counters for one running daemon.
#[derive(Debug, Default)]
pub struct Metrics {
    /// `"GET /v1/describe"` → how many times.
    ///
    /// Keyed by the matched *route template*, not the raw path: a thousand sessions would otherwise
    /// be a thousand keys, and the counter would be a memory leak shaped like a metric.
    requests: Mutex<BTreeMap<String, u64>>,
    /// Refused by authentication.
    unauthorized: AtomicU64,
    /// Sessions opened, agent and process alike.
    sessions: AtomicU64,
    /// Tool sources reloaded.
    reloads: AtomicU64,
    /// Sessions that ended because a budget ran out.
    budget_stops: AtomicU64,
    /// When the daemon started counting, in unix milliseconds.
    since_unix_millis: AtomicU64,
}

impl Metrics {
    /// Start counting from now.
    #[must_use]
    pub fn started_now() -> Self {
        let metrics = Self::default();
        metrics
            .since_unix_millis
            .store(nucleos::adapter::Clock::now_unix_millis(&nucleos::adapter::SystemClock), Ordering::Relaxed);
        metrics
    }

    /// One request, however it ended.
    pub fn observe(&self, method: &str, route: &str, status: u16) {
        let key = format!("{method} {route}");
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *requests.entry(key).or_insert(0) += 1;
        if status == 401 {
            self.unauthorized.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A session was opened.
    pub fn session_opened(&self) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
    }

    /// A source was reloaded.
    pub fn reloaded(&self) {
        self.reloads.fetch_add(1, Ordering::Relaxed);
    }

    /// A session ended because it ran out of budget.
    pub fn budget_stopped(&self) {
        self.budget_stops.fetch_add(1, Ordering::Relaxed);
    }

    /// The counters, as the wire reports them.
    #[must_use]
    pub fn snapshot(&self) -> MetricsResponse {
        let requests = self
            .requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default();
        let now = nucleos::adapter::Clock::now_unix_millis(&nucleos::adapter::SystemClock);
        MetricsResponse {
            requests,
            unauthorized: self.unauthorized.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            reloads: self.reloads.load(Ordering::Relaxed),
            budget_stops: self.budget_stops.load(Ordering::Relaxed),
            uptime_millis: now.saturating_sub(self.since_unix_millis.load(Ordering::Relaxed)),
        }
    }
}

/// One JSON line per request, for a collector that would rather parse than poll.
///
/// Written to stdout rather than through a logging crate: there is one format, one sink and one
/// level, and a dependency that exists to print one line is a dependency that will grow a
/// configuration language.
#[must_use]
pub fn line(method: &str, route: &str, status: u16, millis: u64) -> String {
    serde_json::json!({
        "at_unix_millis": nucleos::adapter::Clock::now_unix_millis(&nucleos::adapter::SystemClock),
        "kind": "request",
        "method": method,
        "route": route,
        "status": status,
        "duration_millis": millis,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn requests_are_counted_by_route_and_refusals_apart() {
        let metrics = Metrics::started_now();
        metrics.observe("GET", "/v1/describe", 200);
        metrics.observe("GET", "/v1/describe", 200);
        metrics.observe("GET", "/v1/sessions/{id}", 401);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests.get("GET /v1/describe"), Some(&2));
        assert_eq!(snapshot.unauthorized, 1);
        assert_eq!(snapshot.requests.len(), 2, "one key per route, not per path");
    }

    #[test]
    fn the_line_is_json_a_collector_can_parse() {
        let line = line("POST", "/v1/sessions", 200, 12);
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["route"], "/v1/sessions");
        assert_eq!(parsed["status"], 200);
        assert_eq!(parsed["kind"], "request");
    }
}
