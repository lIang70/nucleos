//! Retention: the log is kept for as long as it is useful, not forever.
//!
//! A session's log is the record of its work, and a closed session's log is history. History is worth
//! keeping — but "everything, forever" is not a retention policy, it is an accident that happens to a
//! disk. This module is the deployment's answer: remove closed sessions once they have been idle long
//! enough that nobody is likely to replay them.
//!
//! Two rules shape it. **Only closed sessions are removed**: an open session is work in progress, and a
//! parked one is waiting on a human. **A session whose activity is unknown is kept**: retention deletes
//! what it can date, never what it cannot.

use crate::admin::Admin;
use crate::http::{ApiError, App};
use axum::Json;
use axum::extract::State;
use nucleos::adapter::{ConfigScope, PruneQuery, Verdict};
use nucleos::error::Error;
use nucleos::wire::api::{PruneRequest, PruneResponse};
use std::sync::Arc;

/// The largest single pass. A bound, so retention is a batch and not an unbounded delete.
const DEFAULT_LIMIT: usize = 1_000;

/// How many sessions a report names, so an operator can check before running it for real.
const SAMPLE: usize = 50;

/// Remove closed sessions idle past a deadline.
///
/// `dry_run` defaults to true: a destructive call has to say so explicitly, and a request that forgets
/// is a report, not a deletion.
pub async fn prune(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> Result<Json<PruneResponse>, ApiError> {
    // The `Admin` extractor has already read the body to verify the signature, so the request is
    // parsed from what it checked rather than extracted a second time.
    let request: PruneRequest = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the prune request is not valid JSON: {error}"
        )))
    })?;
    let kernel = app.kernel()?.clone();
    // A signature says who; the gate says whether they may do this. Deleting other people's history is
    // a configuration act, and it goes through the same gate as every other one.
    match kernel
        .check_config(&admin.principal, ConfigScope::System, "sessions")
        .await
        .map_err(|error| ApiError::from_error(&error))?
    {
        Verdict::Allow => {}
        Verdict::Deny { code, reason } => {
            return Err(ApiError::from_error(&Error::new(code, reason)));
        }
        Verdict::AskHuman { question, .. } => {
            return Err(ApiError::from_error(&Error::denied(format!(
                "retention needs a human ({question}), and a prune cannot wait for one"
            ))));
        }
    }

    let before = match (request.before_unix_millis, request.older_than_millis) {
        (Some(before), None) => before,
        (None, Some(age)) => kernel.now().saturating_sub(age),
        _ => {
            return Err(ApiError::from_error(&Error::invalid(
                "name exactly one of `before_unix_millis` and `older_than_millis`",
            )));
        }
    };
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).min(DEFAULT_LIMIT);
    let dry_run = request.dry_run;

    let report = kernel
        .prune_closed(&PruneQuery {
            tenant: request.tenant,
            before_unix_millis: before,
            limit,
            dry_run,
        })
        .await
        .map_err(|error| ApiError::from_error(&error))?;

    if !dry_run {
        app.metrics()
            .retention_pruned(report.sessions, report.events);
    }
    tracing::info!(
        event = "retention_pruned",
        sessions = report.sessions,
        events = report.events,
        before_unix_millis = before,
        dry_run,
        by = %admin.principal.subject,
    );

    Ok(Json(PruneResponse {
        sessions: report.sessions,
        events: report.events,
        dry_run,
        sample: report
            .candidates
            .iter()
            .take(SAMPLE)
            .map(|key| key.id.as_str().to_owned())
            .collect(),
    }))
}

/// One pass of the sweeper, over whatever the configuration says right now.
///
/// Returns `None` when retention is not configured, or when it is configured without both a TTL and
/// an interval — which is also what a disabled sweeper means, so the loop and the tests share one
/// definition of "nothing to do".
pub async fn sweep_once(app: &Arc<App>) -> Result<Option<nucleos::adapter::PruneReport>, Error> {
    let Some(system) = app.system() else {
        return Ok(None);
    };
    let Some(retention) = crate::configure::configured_retention(system).await? else {
        return Ok(None);
    };
    let (Some(ttl), Some(_interval)) = (
        retention.closed_session_ttl_millis,
        retention.sweep_interval_millis,
    ) else {
        return Ok(None);
    };
    let kernel = app.kernel().map_err(|error| error.into_error())?;
    let before = kernel.now().saturating_sub(ttl);
    let report = kernel
        .prune_closed(&PruneQuery {
            tenant: None,
            before_unix_millis: before,
            limit: DEFAULT_LIMIT,
            dry_run: false,
        })
        .await?;
    if report.sessions > 0 {
        app.metrics()
            .retention_pruned(report.sessions, report.events);
        tracing::info!(
            event = "retention_swept",
            sessions = report.sessions,
            events = report.events,
            before_unix_millis = before,
        );
    }
    Ok(Some(report))
}

/// How often a disabled sweeper looks at the configuration again.
///
/// A minute: cheap, and a deployment that turns retention on does not wait long for it to take effect.
const DISABLED_POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Run the retention sweeper until `stop` says the daemon is going away.
///
/// The configuration is read on every pass, so `PUT /v1/config` takes effect on the next one — no
/// restart, and no extra wiring between the configuration surface and this task.
pub fn spawn(
    app: Arc<App>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let interval = match app.system() {
                Some(system) => crate::configure::configured_retention(system)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|retention| retention.sweep_interval_millis),
                None => None,
            };
            match interval {
                Some(millis) if millis > 0 => {
                    tokio::select! {
                        () = tokio::time::sleep(std::time::Duration::from_millis(millis)) => {}
                        _ = stop.changed() => return,
                    }
                    if *stop.borrow() {
                        return;
                    }
                    if let Err(error) = sweep_once(&app).await {
                        // A failed pass is a warning and a later retry, not a dead sweeper: a store
                        // that is briefly unavailable should not turn retention off for good.
                        tracing::warn!(event = "retention_sweep_failed", detail = %error);
                    }
                }
                _ => {
                    tokio::select! {
                        () = tokio::time::sleep(DISABLED_POLL) => {}
                        _ = stop.changed() => return,
                    }
                    if *stop.borrow() {
                        return;
                    }
                }
            }
        }
    })
}
