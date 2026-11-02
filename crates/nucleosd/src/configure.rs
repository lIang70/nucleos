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
use axum::Json;
use axum::extract::{Path, State};
use nucleos::adapter::{ConfigScope, Verdict};
use nucleos::error::{Error, Result};
use nucleos::wire::api::{
    AdminKeyListResponse, AdminKeyRequest, ConfigKey, ModelListResponse, ModelRequest, ModelView,
    PutResponse, TenantLimitsRequest, TenantLimitsView,
};
use nucleos_sqlite::{Change, SystemStore};
use std::sync::Arc;

/// Where one tenant's policy lives. The prefix is what `with_prefix` scans.
pub const TENANT_POLICY_PREFIX: &str = "config/policy/tenant/";

/// Where one model's sealed API key lives, one row each.
///
/// Separate from the model definition, which is reported: a definition is configuration and a sealed
/// key is a credential, and the endpoint that lists definitions must not be the endpoint that hands out
/// credentials — not even encrypted ones.
pub const SEALED_KEY_PREFIX: &str = "config/keys/model/";

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
    let document =
        nucleos_policy::Document::parse(&text).map_err(|error| ApiError::from_error(&error))?;

    permit(&app, &admin, "policy").await?;
    let ruleset = nucleos_policy::Ruleset::new(document.clone())
        .map_err(|error| ApiError::from_error(&error))?;

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

    // The key is either sealed into the store or read from a file the operator protects. A plaintext key
    // in the store would make every backup a credential dump, and there is no third option.
    let sealed = match &request.key {
        Some(key) => {
            let seal = app.seal().ok_or_else(|| {
                ApiError::from_error(&Error::unavailable(
                    "no seal key is configured, so an API key cannot be stored: pass a key_file, or \
                     create a seal key with `nucleos init`",
                ))
            })?;
            Some(
                seal.seal(key)
                    .map_err(|error| ApiError::from_error(&error))?,
            )
        }
        None => None,
    };
    let adapter =
        adapter_for(&request, request.key.clone()).map_err(|error| ApiError::from_error(&error))?;
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
        has_sealed_key: sealed.is_some(),
        strict_tools: request.strict_tools,
        reasoning: request.reasoning,
    });
    if let Some(sealed) = sealed {
        system(&app)?
            .set(&format!("{SEALED_KEY_PREFIX}{}", request.name), &sealed)
            .await
            .map_err(|error| ApiError::from_error(&error))?;
    }
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
    serde_json::from_str(&raw).map_err(|error| {
        Error::internal(format!("stored model definitions are unreadable: {error}"))
    })
}

/// Build the provider adapter for one definition.
///
/// `api_key` is passed in rather than read here: at add time it came from the request and was sealed, at
/// start-up it comes from the sealed row. One function, two callers, one way to disagree about which key
/// a model uses.
pub fn adapter_for(
    request: &ModelRequest,
    api_key: Option<String>,
) -> Result<nucleos_openai::OpenAi> {
    let api_key = match (api_key, &request.key_file) {
        (Some(key), _) => Some(key),
        // A path is the alternative to sealing, not a fallback for it: a credential with two sources has
        // one that nobody rotates.
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| {
                    Error::invalid(format!("cannot read the key file {path}: {error}"))
                })?
                .trim()
                .to_owned(),
        ),
        (None, None) => None,
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

/// Replace one tenant's policy.
///
/// The tenant's document cannot lighten the deployment's: both are evaluated and the stricter answer
/// wins (see `nucleos_policy::Layered`). This endpoint is how a tenant gets a policy at all — and the
/// gate is what decides *who* may write it, with `scope: "own"` meaning "an administrator of that
/// tenant and nobody else".
pub async fn put_tenant_policy(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<PutResponse>, ApiError> {
    let kernel = app.kernel()?;
    let text = String::from_utf8(admin.body.to_vec()).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the policy must be UTF-8 text: {error}"
        )))
    })?;
    let document =
        nucleos_policy::Document::parse(&text).map_err(|error| ApiError::from_error(&error))?;
    if tenant.trim().is_empty() {
        return Err(ApiError::from_error(&Error::invalid(
            "a tenant policy needs a tenant",
        )));
    }

    permit_scope(&app, &admin, ConfigScope::Tenant(&tenant), "policy").await?;

    let system = system(&app)?;
    system
        .set(&format!("{TENANT_POLICY_PREFIX}{tenant}"), &text)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    // The whole policy is rebuilt and swapped: a tenant policy is not a patch on the deployment's, it
    // is a second answer that has to be composed with it on every call.
    let layered = layered_from(system).map_err(|error| ApiError::from_error(&error))?;
    kernel
        .set_policy(Arc::new(layered))
        .map_err(|error| ApiError::from_error(&error))?;
    record(&app, &admin, "tenants", &format!("policy {tenant}")).await?;

    tracing::info!(
        event = "tenant_policy_replaced",
        tenant = %tenant,
        rules = document.rules.len(),
        by = %admin.principal.subject,
    );
    Ok(Json(PutResponse {
        changed: format!("policy/{tenant}"),
        rules: document.rules.len(),
    }))
}

/// Where one tenant's limits live.
pub const TENANT_LIMITS_PREFIX: &str = "config/limits/tenant/";

/// Set what one tenant may use.
///
/// Applied to the running kernel before it is stored, like every other change: a daemon reporting a
/// limit it does not enforce is the failure mode this order exists to avoid.
pub async fn put_tenant_limits(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<TenantLimitsView>, ApiError> {
    let request: TenantLimitsRequest = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the limits are not valid JSON: {error}"
        )))
    })?;
    if tenant.trim().is_empty() {
        return Err(ApiError::from_error(&Error::invalid(
            "tenant limits need a tenant",
        )));
    }
    permit_scope(&app, &admin, ConfigScope::Tenant(&tenant), "limits").await?;

    let kernel = app.kernel()?;
    let mut view = stored_limits(&app, &tenant)?;
    // Absent leaves it unchanged; `null` clears; an array sets — including an empty one, which means
    // "nothing", not "no limit".
    if let Some(tools) = &request.tools {
        kernel
            .set_tenant_tools(&tenant, tools.clone())
            .map_err(|error| ApiError::from_error(&error))?;
        view.tools = tools.clone();
    }
    if let Some(models) = &request.models {
        kernel
            .set_tenant_models(&tenant, models.clone())
            .map_err(|error| ApiError::from_error(&error))?;
        view.models = models.clone();
    }

    let text = serde_json::to_string(&view).map_err(|error| {
        ApiError::from_error(&Error::internal(format!(
            "cannot encode the limits: {error}"
        )))
    })?;
    system(&app)?
        .set(&format!("{TENANT_LIMITS_PREFIX}{tenant}"), &text)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    record(&app, &admin, "limits", &tenant).await?;

    tracing::info!(
        event = "tenant_limits_set",
        tenant = %tenant,
        tools = ?view.tools,
        models = ?view.models,
        by = %admin.principal.subject,
    );
    Ok(Json(view))
}

/// Apply every stored tenant limit to a kernel, at start-up.
pub fn apply_tenant_limits(kernel: &nucleos::Kernel, system: &SystemStore) -> Result<()> {
    for (key, value) in system.with_prefix(TENANT_LIMITS_PREFIX)? {
        let Some(tenant) = key.strip_prefix(TENANT_LIMITS_PREFIX) else {
            continue;
        };
        let view: nucleos::wire::api::TenantLimitsView =
            serde_json::from_str(&value).map_err(|error| {
                Error::internal(format!(
                    "stored limits for {tenant:?} are unreadable: {error}"
                ))
            })?;
        kernel.set_tenant_tools(tenant, view.tools)?;
        kernel.set_tenant_models(tenant, view.models)?;
    }
    Ok(())
}

fn stored_limits(app: &Arc<App>, tenant: &str) -> std::result::Result<TenantLimitsView, ApiError> {
    let Some(raw) = system(app)?
        .get(&format!("{TENANT_LIMITS_PREFIX}{tenant}"))
        .map_err(|error| ApiError::from_error(&error))?
    else {
        return Ok(TenantLimitsView {
            tenant: tenant.to_owned(),
            tools: None,
            models: None,
        });
    };
    serde_json::from_str(&raw).map_err(|error| {
        ApiError::from_error(&Error::internal(format!(
            "stored limits for {tenant:?} are unreadable: {error}"
        )))
    })
}

/// The policy in force, rebuilt from the store: the deployment's and every tenant's.
pub fn layered_from(system: &SystemStore) -> Result<nucleos_policy::Layered> {
    let text = system
        .get(POLICY_KEY)?
        .ok_or_else(|| Error::invalid("this deployment has no policy: run `nucleos init`"))?;
    let mut layered = nucleos_policy::Layered::system(nucleos_policy::Document::parse(&text)?)?;
    for (key, value) in system.with_prefix(TENANT_POLICY_PREFIX)? {
        let Some(tenant) = key.strip_prefix(TENANT_POLICY_PREFIX) else {
            continue;
        };
        layered = layered.with_tenant(tenant, nucleos_policy::Document::parse(&value)?)?;
    }
    Ok(layered)
}

/// Ask the gate about one scope and subject.
async fn permit_scope(
    app: &Arc<App>,
    admin: &Admin,
    scope: ConfigScope<'_>,
    subject: &str,
) -> std::result::Result<(), ApiError> {
    let kernel = app.kernel()?;
    match kernel
        .check_config(&admin.principal, scope, subject)
        .await
        .map_err(|error| ApiError::from_error(&error))?
    {
        Verdict::Allow => Ok(()),
        Verdict::Deny { code, reason } => Err(ApiError::from_error(&Error::new(code, reason))),
        Verdict::AskHuman { question, .. } => Err(ApiError::from_error(&Error::denied(format!(
            "changing {subject} needs a human ({question}), and configuration cannot wait for one"
        )))),
    }
}

/// The administrators this deployment trusts.
pub async fn list_admin_keys(
    State(app): State<Arc<App>>,
    _admin: Admin,
) -> std::result::Result<Json<AdminKeyListResponse>, ApiError> {
    Ok(Json(AdminKeyListResponse {
        keys: enrolled(&app)?
            .iter()
            .map(|key| ConfigKey {
                id: key.id.clone(),
                tenant: key.tenant.clone(),
                subject: key.subject.clone(),
            })
            .collect(),
        warning: None,
    }))
}

/// Enrol another administrator's public key.
///
/// The private half never travels: this endpoint takes the verifying half, which is why enrolling is
/// safe over the wire and why the daemon can never mint administration for itself.
pub async fn post_admin_key(
    State(app): State<Arc<App>>,
    admin: Admin,
) -> std::result::Result<Json<AdminKeyListResponse>, ApiError> {
    let request: AdminKeyRequest = serde_json::from_slice(&admin.body).map_err(|error| {
        ApiError::from_error(&Error::invalid(format!(
            "the key is not valid JSON: {error}"
        )))
    })?;
    permit(&app, &admin, "admin_keys").await?;

    let key = nucleos_auth::PublicKey::from_public(
        &request.key,
        request.tenant.clone(),
        request.subject.clone(),
    )
    .map_err(|error| ApiError::from_error(&error))?;

    let mut keys = enrolled(&app)?;
    if keys.iter().any(|existing| existing.id == key.id) {
        return Err(ApiError::from_error(&Error::invalid(format!(
            "key {} is already enrolled",
            key.id
        ))));
    }
    keys.push(key.clone());
    persist_keys(&app, &keys, &admin, &format!("enrol {}", key.id)).await?;

    tracing::info!(
        event = "admin_key_enrolled",
        id = %key.id,
        subject = %key.subject,
        by = %admin.principal.subject,
    );
    Ok(Json(AdminKeyListResponse {
        keys: keys.iter().map(view).collect(),
        warning: None,
    }))
}

/// Revoke an administrator's key.
///
/// Immediate: the verifier reads the list on every request, so the next request signed with this key
/// is refused. Removing the *last* key is allowed and warned about — locking yourself out is worse than
/// a weaker fallback, and the recovery password still exists for exactly this.
pub async fn delete_admin_key(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    admin: Admin,
) -> std::result::Result<Json<AdminKeyListResponse>, ApiError> {
    permit(&app, &admin, "admin_keys").await?;

    let mut keys = enrolled(&app)?;
    let before = keys.len();
    keys.retain(|key| key.id != id);
    if keys.len() == before {
        return Err(ApiError::from_error(&Error::not_found(format!(
            "no administrator key with id {id:?}"
        ))));
    }
    persist_keys(&app, &keys, &admin, &format!("revoke {id}")).await?;

    let warning = keys.is_empty().then(|| {
        "no administrator key remains: only the recovery password can enrol one now, with \
         `nucleos admin recover`"
            .to_owned()
    });
    tracing::info!(
        event = "admin_key_revoked",
        id = %id,
        remaining = keys.len(),
        by = %admin.principal.subject,
    );
    Ok(Json(AdminKeyListResponse {
        keys: keys.iter().map(view).collect(),
        warning,
    }))
}

fn view(key: &nucleos_auth::PublicKey) -> ConfigKey {
    ConfigKey {
        id: key.id.clone(),
        tenant: key.tenant.clone(),
        subject: key.subject.clone(),
    }
}

fn enrolled(app: &Arc<App>) -> std::result::Result<Vec<nucleos_auth::PublicKey>, ApiError> {
    let Some(raw) = system(app)?
        .get(crate::admin::ADMIN_KEYS_KEY)
        .map_err(|error| ApiError::from_error(&error))?
    else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|error| {
        ApiError::from_error(&Error::internal(format!(
            "stored administrator keys are unreadable: {error}"
        )))
    })
}

async fn persist_keys(
    app: &Arc<App>,
    keys: &[nucleos_auth::PublicKey],
    admin: &Admin,
    what: &str,
) -> std::result::Result<(), ApiError> {
    let text = serde_json::to_string(keys).map_err(|error| {
        ApiError::from_error(&Error::internal(format!(
            "cannot encode the administrator keys: {error}"
        )))
    })?;
    system(app)?
        .set(crate::admin::ADMIN_KEYS_KEY, &text)
        .await
        .map_err(|error| ApiError::from_error(&error))?;
    record(app, admin, "admin_keys", what).await
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
    let request: nucleos::wire::api::TokenIssueRequest = serde_json::from_slice(&admin.body)
        .map_err(|error| {
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
    record(
        &app,
        &admin,
        "tokens",
        &format!("issue {}", request.subject),
    )
    .await?;

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
    app.system()
        .ok_or_else(|| ApiError::from_error(&Error::unavailable("this daemon has no system store")))
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

/// The whole configuration, read out of the store as one document.
///
/// Built from the store rather than from what the daemon is holding: an export that described the
/// running state would be an export that could not reproduce it, and the two differ exactly when
/// something went wrong.
pub fn document(system: &SystemStore) -> Result<nucleos::wire::api::Deployment> {
    use nucleos::wire::api::{Deployment, TenantDocument, TenantLimitsView};

    let policy = system.get(crate::http::POLICY_KEY)?;
    let models = stored_models(system)?;
    let sources: Vec<crate::sources::SourceSpec> = match system.get(crate::sources::SOURCES_KEY)? {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|error| Error::internal(format!("stored sources are unreadable: {error}")))?,
        None => Vec::new(),
    };

    let mut tenants: std::collections::BTreeMap<String, TenantDocument> =
        std::collections::BTreeMap::new();
    for (key, value) in system.with_prefix(TENANT_POLICY_PREFIX)? {
        if let Some(tenant) = key.strip_prefix(TENANT_POLICY_PREFIX) {
            tenants.entry(tenant.to_owned()).or_default().policy = Some(value);
        }
    }
    for (key, value) in system.with_prefix(TENANT_LIMITS_PREFIX)? {
        let Some(tenant) = key.strip_prefix(TENANT_LIMITS_PREFIX) else {
            continue;
        };
        let limits: TenantLimitsView = serde_json::from_str(&value).map_err(|error| {
            Error::internal(format!(
                "stored limits for {tenant} are unreadable: {error}"
            ))
        })?;
        tenants.entry(tenant.to_owned()).or_default().limits = Some(limits);
    }

    Ok(Deployment {
        document_version: 1,
        revision: system.revision()?,
        policy,
        models,
        sources: sources
            .into_iter()
            .map(|spec| nucleos::wire::api::SourceView {
                name: spec.name,
                command: spec.command,
                args: spec.args,
                tools: Vec::new(),
            })
            .collect(),
        tenants,
    })
}

/// What an apply did.
#[derive(Debug, Clone)]
pub struct Applied {
    /// The revision the deployment is now at.
    pub revision: u64,
    /// What changed, in the words the history row uses.
    pub summary: String,
}

/// Apply a document: validate all of it, replace the store in one transaction, then make the running
/// daemon match.
///
/// **Validate before writing anything.** A document that is half applied is a deployment referring to a
/// model nobody defined, and the window between two writes is exactly when a request arrives. The order
/// is: parse every part, build the whole key set, then one transaction.
///
/// **The running daemon is updated after the store**, not before. If rehydration fails the store is
/// still the truth and a restart converges; the opposite order leaves a daemon enforcing something that
/// a restart would silently undo.
pub async fn apply(
    app: &Arc<crate::http::App>,
    system: &SystemStore,
    document: nucleos::wire::api::Deployment,
    expected_revision: Option<u64>,
    admin: &crate::admin::Admin,
) -> Result<Applied> {
    let current = system.revision()?;
    if let Some(expected) = expected_revision
        && expected != current
    {
        return Err(Error::new(
            nucleos::contract::ErrorCode::Conflict,
            format!(
                "this deployment is at revision {current}, the document was written against {expected}"
            ),
        ));
    }
    if document.document_version != 1 {
        return Err(Error::invalid(format!(
            "document_version {} is not one this build understands (1)",
            document.document_version
        )));
    }

    // --- validate -----------------------------------------------------------------------------
    //
    // A document must carry a policy. Not because it is convenient to require one, but because the
    // store's own rule is that a deployment without a policy is not a deployment — deny-by-default is
    // only safe when something says so — and an apply that deleted the policy would produce exactly
    // that state, one request later.
    let Some(policy) = &document.policy else {
        return Err(Error::invalid(
            "the document carries no policy; a deployment without one denies everything, and \
             `nucleos init` writes the bootstrap policy this is meant to replace",
        ));
    };
    nucleos_policy::Document::parse(policy).map_err(|error| {
        Error::invalid(format!(
            "the system policy is not a valid document: {error}"
        ))
    })?;
    let mut names = std::collections::BTreeSet::new();
    for model in &document.models {
        if model.name.trim().is_empty()
            || model.url.trim().is_empty()
            || model.model.trim().is_empty()
        {
            return Err(Error::invalid(format!(
                "model {:?} needs a name, a url and a provider model",
                model.name
            )));
        }
        if !names.insert(model.name.clone()) {
            return Err(Error::invalid(format!(
                "two models are called {:?}",
                model.name
            )));
        }
    }
    let mut source_names = std::collections::BTreeSet::new();
    for source in &document.sources {
        if source.name.trim().is_empty() || source.command.trim().is_empty() {
            return Err(Error::invalid(format!(
                "source {:?} needs a name and a command",
                source.name
            )));
        }
        if !source_names.insert(source.name.clone()) {
            return Err(Error::invalid(format!(
                "two sources are called {:?}",
                source.name
            )));
        }
    }
    for (tenant, part) in &document.tenants {
        if let Some(policy) = &part.policy {
            nucleos_policy::Document::parse(policy).map_err(|error| {
                Error::invalid(format!(
                    "the policy for {tenant} is not a valid document: {error}"
                ))
            })?;
        }
    }

    // --- one transaction ----------------------------------------------------------------------
    let revision = current.saturating_add(1);
    let mut entries: Vec<(String, String)> = Vec::new();
    entries.push((crate::http::POLICY_KEY.to_owned(), policy.clone()));
    entries.push((
        MODELS_KEY.to_owned(),
        serde_json::to_string(&document.models)
            .map_err(|error| Error::internal(format!("models are not storable: {error}")))?,
    ));
    let specs: Vec<crate::sources::SourceSpec> = document
        .sources
        .iter()
        .map(|source| crate::sources::SourceSpec {
            name: source.name.clone(),
            command: source.command.clone(),
            args: source.args.clone(),
            env: std::collections::BTreeMap::new(),
        })
        .collect();
    entries.push((
        crate::sources::SOURCES_KEY.to_owned(),
        serde_json::to_string(&specs)
            .map_err(|error| Error::internal(format!("sources are not storable: {error}")))?,
    ));
    for (tenant, part) in &document.tenants {
        if let Some(policy) = &part.policy {
            entries.push((format!("{TENANT_POLICY_PREFIX}{tenant}"), policy.clone()));
        }
        if let Some(limits) = &part.limits {
            entries.push((
                format!("{TENANT_LIMITS_PREFIX}{tenant}"),
                serde_json::to_string(limits).map_err(|error| {
                    Error::internal(format!("limits for {tenant} are not storable: {error}"))
                })?,
            ));
        }
    }
    entries.push(("config/revision".to_owned(), revision.to_string()));
    system.replace(entries).await?;

    let summary = format!(
        "revision {revision}: {} model(s), {} source(s), {} tenant(s)",
        document.models.len(),
        document.sources.len(),
        document.tenants.len(),
    );
    system
        .record(nucleos_sqlite::Change {
            at_unix_millis: now(),
            tenant: admin.principal.tenant.clone(),
            subject: admin.principal.subject.clone(),
            key_id: admin.key_id.clone(),
            key: "deployment".to_owned(),
            value: summary.clone(),
        })
        .await?;

    // --- make the daemon match ----------------------------------------------------------------
    rehydrate(app, system, &document).await?;

    tracing::info!(
        event = "deployment_applied",
        revision,
        key_id = %admin.key_id,
        models = document.models.len(),
        sources = document.sources.len(),
        tenants = document.tenants.len(),
    );
    Ok(Applied { revision, summary })
}

/// Bring the running daemon in line with the store.
async fn rehydrate(
    app: &Arc<crate::http::App>,
    system: &SystemStore,
    document: &nucleos::wire::api::Deployment,
) -> Result<()> {
    let kernel = app.kernel().map_err(|error| error.into_error())?;

    // Policy first: everything else is checked against it, and a document that widens permissions must
    // widen them before the models it names are reachable.
    let layered = layered_from(system)?;
    kernel.set_policy(Arc::new(layered))?;

    // Models: remove what the document no longer names, then add or replace what it does. Replacing
    // unconditionally is simpler than diffing fields and has the same effect — a model is a route
    // target, not a conversation.
    let wanted: std::collections::BTreeSet<String> = document
        .models
        .iter()
        .map(|model| model.name.clone())
        .collect();
    for existing in kernel.models() {
        if !wanted.contains(&existing) {
            kernel.remove_model(&existing)?;
        }
    }
    for model in &document.models {
        let api_key = match app.seal() {
            Some(seal) => match system.get(&format!("{SEALED_KEY_PREFIX}{}", model.name))? {
                Some(sealed) => Some(seal.open(&sealed)?),
                None => None,
            },
            None => None,
        };
        let adapter = adapter_for(
            &nucleos::wire::api::ModelRequest {
                name: model.name.clone(),
                url: model.url.clone(),
                model: model.model.clone(),
                key_file: model.key_file.clone(),
                context_window: Some(model.context_window),
                strict_tools: model.strict_tools,
                reasoning: model.reasoning,
                ..Default::default()
            },
            api_key,
        )?;
        kernel.add_model(&model.name, Arc::new(adapter))?;
    }

    apply_tenant_limits(kernel, system)?;

    // Sources: reconnected from the store, then published as one generation. A source that cannot
    // start is logged and skipped, the same way start-up treats it.
    let reloaded = crate::sources::SourceSet::load(system).await?;
    *app.sources().write().await = reloaded;
    crate::sources::publish(app, kernel)
        .await
        .map_err(|error| error.into_error())?;
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
