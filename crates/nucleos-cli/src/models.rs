//! `nucleos models …`: the first mutating commands.
//!
//! Each one is a signed request, and none of them writes a file: the daemon applies the change, stores
//! it and records it, and the CLI reports what came back. That is the split §1 of `docs/config.md`
//! argues for — the CLI can create a deployment, and after that it can only ask.

use crate::remote::{Overrides, Remote};
use nucleos::error::Result;
use nucleos::wire::api::{ModelListResponse, ModelRequest};

/// Add or replace a model.
pub fn add(overrides: &Overrides, request: &ModelRequest) -> Result<String> {
    let body = serde_json::to_vec(request).map_err(|error| {
        nucleos::Error::invalid(format!("the model definition is not encodable: {error}"))
    })?;
    Remote::open(overrides)?.call("POST", "/v1/models", &body)
}

/// List the models this deployment has.
pub fn list(overrides: &Overrides) -> Result<String> {
    let body = Remote::open(overrides)?.call("GET", "/v1/models", b"")?;
    let list: ModelListResponse = serde_json::from_str(&body).map_err(|error| {
        nucleos::Error::internal(format!("the daemon's model list is unreadable: {error}"))
    })?;
    if list.models.is_empty() {
        return Ok("no models".to_owned());
    }
    let mut out = String::new();
    for model in list.models {
        out.push_str(&format!(
            "{:<12} {:<40} {} ({})\n",
            model.name,
            model.url,
            model.model,
            model.context_window
        ));
    }
    Ok(out.trim_end().to_owned())
}

/// Remove a model.
pub fn remove(overrides: &Overrides, name: &str) -> Result<String> {
    Remote::open(overrides)?.call("DELETE", &format!("/v1/models/{name}"), b"")
}
