//! Configuration changes: the surface the signature and the gate were built for.
//!
//! Three rules run through every handler here, in this order, and the order is the design:
//!
//! 1. **The signature** (the `Admin` extractor) says *who*. Without it, nothing below runs.
//! 2. **The gate** (`Kernel::check_config`) says *whether this administrator may change this*. A
//!    signature proves identity; the policy proves permission, and the two are different questions.
//! 3. **The record** — the old value is replaced in the system store and the change is appended to
//!    its history. A configuration that changed without a line somewhere is, afterwards,
//!    indistinguishable from a bug.
//!
//! Everything is applied to the running kernel before it is stored, so a store failure leaves a
//! daemon enforcing something it cannot explain — which is the smaller problem than a daemon that
//! reports a change it never made. The order is stated here because it is a choice, not an accident.
//!
//! ## What a change may contain
//!
//! No secrets. A model takes a `key_file` — a path — rather than an API key, because the system store
//! is backed up and copied, and a credential in it would make a backup a credential dump. Sealed
//! credentials are §4 of `docs/config.md` and are not built.

use crate::admin::Admin;
use crate::http::{ApiError, App, POLICY_KEY};
use axum::extract::{Path, State};
use axum::Json;
use nucleos::adapter::{ConfigScope, Verdict};
use nucleos::error::{Error, Result};
use nucleos::wire::api::{ModelListResponse, ModelRequest, ModelView, PutResponse};
use nucleos_sqlite::{Change, SystemStore};
use std::sync::Arc;

/// Where model definitions live in the system store.
pub const MODELS_KEY: &str = "config/models";

/// Replace the system policy.
pub async fn put_policy(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> std::result::Result<Json<PutResponse>, ApiError> {
    let kernel = app.kernel()?;
    let text = String::from_utf8(admin.body.to_vec()).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the policy must be UTF-8 text: {error}"
        )))
    })?;

    // Parsed and validated before the gate: a document that cannot mean what it says is a caller
    // error, and asking the policy whether an invalid policy may replace it is a question with no
    // useful answer.
    let document = nucleos_policy::Document::parse(&text).map_err(|error| {
        ApiError::from_error(&error)
    })?;

    permit(&app, &admin, "policy").await?;
    let ruleset = nucleos_policy::Ruleset::new(document.clone()).map_err(|error| {
        ApiError::from_error(&error)
    })?;

    // Applied first, then stored: a daemon enforcing a policy it cannot explain is a smaller problem
    // than one reporting a change that never took effect.
    kernel
        .set_policy(Arc::new(ruleset))
        .map_err(|error| ApiError::from_error(&error))?;
    let system = system(&app)?;
    system
        .set(POLICY_KEY, &text)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    record(&app, &admin, "policy", &text).await?;

    tracing::info!(
        event = "policy_replaced",
        by = %admin.principal.subject,
        default = ?document.unmatched,
        rules = document.rules.len(),
    );
    Ok(Json(PutResponse {
        changed: "policy".to_owned(),
        rules: document.rules.len(),
    }))
}

/// Add or replace a model.
pub async fn post_model(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> std::result::Result<Json<ModelView>, ApiError> {
    let kernel = app.kernel()?;
    let request: ModelRequest = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the model definition is not valid JSON: {error}"
        )))
    })?;
    if request.name.trim().is_empty() || request.url.trim().is_empty() {
        return Err(ApiError::from_error(&Error::invalid(
            "a model needs a name and a url",
        )));
    }

    permit(&app, &admin, "models").await?;

    // The key is a *path*, never a value: the system store is backed up and copied, and a backup that
    // contains an API key is a credential dump. Sealed storage is designed and not built.
    let adapter = adapter_for(&request).map_err(|error| ApiError::from_error(&error))?;
    kernel
        .add_model(&request.name, Arc::new(adapter))
        .map_err(|error| ApiError::from_error(&error))?;

    let mut models = models_of(&app)?;
    models.retain(|model| model.name != request.name);
    models.push(ModelView {
        name: request.name.clone(),
        url: request.url.clone(),
        model: request.model.clone(),
        context_window: request.context_window.unwrap_or(128_000),
        key_file: request.key_file.clone(),
        strict_tools: request.strict_tools,
        reasoning: request.reasoning,
    });
    persist_models(&app, &models, &admin).await?;
    // The advertised list is derived, not frozen: a caller reads `describe` before it opens a
    // session, and a stale list is a caller told to go away for no reason.
    app.set_models(models.iter().map(|model| model.name.clone()).collect());

    tracing::info!(event = "model_added", name = %request.name, by = %admin.principal.subject);
    Ok(Json(models.last().cloned().unwrap_or_default()))
}

/// Remove a model.
pub async fn delete_model(
    State(app): State<Arc<App>>,
    // `Path` before `Admin`: the signature extractor reads the body, and axum allows exactly one
    // body consumer, last.
    Path(name): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<ModelListResponse>, ApiError> {
    let kernel = app.kernel()?;
    permit(&app, &admin, "models").await?;

    if !kernel
        .remove_model(&name)
        .map_err(|error| ApiError::from_error(&error))?
    {
        return Err(ApiError::from_error(&Error::not_found(format!(
            "no model named {name:?}"
        ))));
    }
    let mut models = models_of(&app)?;
    models.retain(|model| model.name != name);
    persist_models(&app, &models, &admin).await?;
    app.set_models(models.iter().map(|model| model.name.clone()).collect());

    tracing::info!(event = "model_removed", name = %name, by = %admin.principal.subject);
    Ok(Json(ModelListResponse { models }))
}

/// The models a deployment has stored, ready to be registered at start-up.
///
/// Without this a model added by request would work until the next restart and then vanish — the
/// operator would have been told the change succeeded, and it did, for as long as the process lived.
pub fn stored_models(system: &SystemStore) -> Result<Vec<ModelView>> {
    let Some(raw) = system.get(MODELS_KEY)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw)
        .map_err(|error| Error::internal(format!("stored model definitions are unreadable: {error}")))
}

/// Build the provider adapter for one definition.
///
/// One function for both paths — a request and a restart — because two ways to build a model is two
/// ways to disagree about which key a model uses.
pub fn adapter_for(request: &ModelRequest) -> Result<nucleos_openai::OpenAi> {
    // The key comes from the named file or not at all. A fallback to an environment variable would be
    // a second source for a credential, and the second source is always the one nobody rotates.
    let api_key = match &request.key_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| {
                    Error::invalid(format!("cannot read the key file {path}: {error}"))
                })?
                .trim()
                .to_owned(),
        ),
        None => None,
    };
    nucleos_openai::OpenAi::new(nucleos_openai::Config {
        url: request.url.clone(),
        api_key,
        model: request.model.clone(),
        capability: nucleos_openai::Config::capabilities(
            request.context_window.unwrap_or(128_000),
            8_192,
        ),
        options: nucleos::contract::Hints::default(),
        prompt_cache_key: None,
        include: Vec::new(),
        safety_identifier: None,
        strict_tools: request.strict_tools,
        supports_reasoning: request.reasoning,
        timeout: Some(std::time::Duration::from_secs(120)),
    })
}

/// Issue a tenant token.
///
/// The value is returned once and never stored: the row keeps a hash, so a later read of this endpoint
/// cannot hand it back. That is the only honest shape for a bearer token — one you can retrieve is one
/// that exists in two places.
pub async fn post_token(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> std::result::Result<Json<nucleos::wire::api::TokenIssued>, ApiError> {
    let request: nucleos::wire::api::TokenIssueRequest =
        serde_json::from_slice(&admin.body).map_err(|error| {
            ApiError::from_error(&Error::invalid(format!(
                "the token request is not valid JSON: {error}"
            )))
        })?;
    if request.tenant.trim().is_empty() || request.subject.trim().is_empty() {
        return Err(ApiError::from_error(&Error::invalid(
            "a token needs a tenant and a subject: it is an identity, not a password",
        )));
    }
    permit(&app, &admin, "tokens").await?;

    let issued = nucleos_auth::token::issue();
    // Sticky: from here on this deployment always needs a token, whatever the table holds later.
    system(&app)?
        .require_tokens()
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    system(&app)?
        .add_token(&nucleos_sqlite::IssuedToken {
            id: issued.id.clone(),
            tenant: request.tenant.clone(),
            subject: request.subject.clone(),
            hash: issued.hash.clone(),
            created_unix_millis: now(),
        })
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    record(&app, &admin, "tokens", &format!("issue {}", request.subject)).await?;

    tracing::info!(
        event = "token_issued",
        id = %issued.id,
        tenant = %request.tenant,
        subject = %request.subject,
        by = %admin.principal.subject,
    );
    Ok(Json(nucleos::wire::api::TokenIssued {
        id: issued.id,
        token: issued.token,
        tenant: request.tenant,
        subject: request.subject,
    }))
}

/// The tokens this deployment has issued, without their values.
pub async fn list_tokens(
    State(app): State<Arc<App>>,
    _admin: Admin,
) -> std::result::Result<Json<nucleos::wire::api::TokenListResponse>, ApiError> {
    let tokens = system(&app)?
        .list_tokens()
        .map_err(|error| ApiError::from_error(&error))?;
    Ok(Json(nucleos::wire::api::TokenListResponse {
        tokens: tokens
            .into_iter()
            .map(|token| nucleos::wire::api::TokenView {
                id: token.id,
                tenant: token.tenant,
                subject: token.subject,
                created_unix_millis: token.created_unix_millis,
            })
            .collect(),
    }))
}

/// Revoke a token. Immediate, because a revocation that waits is not a revocation.
pub async fn delete_token(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<nucleos::wire::api::TokenListResponse>, ApiError> {
    permit(&app, &admin, "tokens").await?;
    let removed = system(&app)?
        .revoke_token(&id)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    if !removed {
        return Err(ApiError::from_error(&Error::not_found(format!(
            "no token with id {id:?}"
        ))));
    }
    record(&app, &admin, "tokens", &format!("revoke {id}")).await?;

    tracing::info!(event = "token_revoked", id = %id, by = %admin.principal.subject);
    list_tokens(State(app), admin).await
}

/// The models this deployment has.
pub async fn list_models(
    State(app): State<Arc<App>>,
    _admin: Admin,
) -> std::result::Result<Json<ModelListResponse>, ApiError> {
    Ok(Json(ModelListResponse {
        models: models_of(&app)?,
    }))
}

/// Ask the gate, and refuse in the one way a caller can act on.
async fn permit(app: &Arc<App>, admin: &Admin, subject: &str) -> std::result::Result<(), ApiError> {
    let kernel = app.kernel()?;
    match kernel
        .check_config(&admin.principal, ConfigScope::System, subject)
        .await
        .map_err(|error| ApiError::from_error(&error))?
    {
        Verdict::Allow => Ok(()),
        Verdict::Deny { code, reason } => Err(ApiError::from_error(&Error::new(code, reason))),
        // `check_config` already translates a human question into a refusal; an `ask` reaching here
        // would be a bug in that translation rather than a decision to honour.
        Verdict::AskHuman { question, .. } => Err(ApiError::from_error(&Error::denied(format!(
            "changing {subject} needs a human ({question}), and configuration cannot wait for one"
        )))),
    }
}

fn system(app: &Arc<App>) -> std::result::Result<&Arc<SystemStore>, ApiError> {
    app.system().ok_or_else(|| {
        ApiError::from_error(&Error::unavailable("this daemon has no system store"))
    })
}

fn models_of(app: &Arc<App>) -> std::result::Result<Vec<ModelView>, ApiError> {
    stored_models(system(app)?).map_err(|error| ApiError::from_error(&error))
}

async fn persist_models(
    app: &Arc<App>,
    models: &[ModelView],
    admin: &Admin,
) -> std::result::Result<(), ApiError> {
    let text = serde_json::to_string(models).map_err(|error| {
        ApiError::from_error(&Error::internal(format!(
            "cannot encode the model definitions: {error}"
        )))
    })?;
    system(app)?
        .set(MODELS_KEY, &text)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    record(app, admin, "models", &text).await
}

/// Append the change to the deployment's history.
async fn record(
    app: &Arc<App>,
    admin: &Admin,
    key: &str,
    value: &str,
) -> std::result::Result<(), ApiError> {
    system(app)?
        .record(Change {
            at_unix_millis: now(),
            tenant: admin.principal.tenant.clone(),
            subject: admin.principal.subject.clone(),
            key_id: String::new(),
            key: key.to_owned(),
            value: value.to_owned(),
        })
        .await
        .map_err(|error| ApiError::from_error(&error))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
