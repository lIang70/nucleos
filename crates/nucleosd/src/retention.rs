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
