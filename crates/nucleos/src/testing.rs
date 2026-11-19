//! Checks an adapter can run against itself, before it is wired into a deployment.
//!
//! These are the invariants the kernel relies on, expressed without a kernel or a store. They are
//! deliberately small: the point is to catch the adapter-specific failures — a field the plan forgot,
//! a degradation with no explanation, a stream that never says why it ended — not to reimplement the
//! kernel's turn machinery.

use crate::adapter::{Model, ModelRequest};
use crate::contract::{ErrorCode, Field};
use crate::error::{Error, Result};
use std::collections::BTreeSet;
use tokio_stream::StreamExt;

/// Exercise one model's contract with one request.
///
/// The checks are:
///
/// - every active field is honoured or explained, and no field is invented (foreign extensions are
///   inert, exactly as the kernel treats them);
/// - every degradation has a reason;
/// - the stream can be read to a terminal `Finished` delta.
///
/// # Errors
///
/// The first contract violation, or the first stream error.
pub async fn conformance(model: &dyn Model, request: ModelRequest) -> Result<()> {
    let active = request.active_fields();
    let prepared = model.prepare(request)?;
    let plan = prepared.plan();

    let provider = model.provider_id();
    let own = |field: &Field| match field {
        Field::Extension {
            provider: other, ..
        } => other == provider,
        _ => true,
    };
    let requested: BTreeSet<_> = active.iter().filter(|field| own(field)).cloned().collect();
    let mut explained: BTreeSet<_> = plan
        .honoured
        .iter()
        .filter(|field| own(field))
        .cloned()
        .collect();
    explained.extend(
        plan.degraded
            .iter()
            .map(|entry| entry.field.clone())
            .filter(|field| own(field)),
    );
    if let Some(missing) = requested.difference(&explained).next() {
        return Err(Error::new(
            ErrorCode::AdapterContractViolation,
            format!("the adapter neither honoured nor explained `{missing}`"),
        ));
    }
    if let Some(invented) = explained.difference(&requested).next() {
        return Err(Error::new(
            ErrorCode::AdapterContractViolation,
            format!("the adapter claims `{invented}`, which the request never set"),
        ));
    }
    for entry in &plan.degraded {
        entry
            .validate()
            .map_err(|reason| Error::new(ErrorCode::AdapterContractViolation, reason))?;
    }

    // The same guard the kernel runs, not a second opinion: a rule enforced only here is a rule a
    // real adapter breaks in production.
    let mut stream = model.invoke(prepared).await?;
    let mut guard = crate::adapter::DeltaGuard::new();
    while let Some(delta) = stream.next().await {
        guard.accept(&delta?)?;
    }
    if !guard.finished() {
        return Err(Error::new(
            ErrorCode::ProviderTruncated,
            "the model stream ended without a finish reason",
        ));
    }
    Ok(())
}
