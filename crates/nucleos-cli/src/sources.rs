//! `nucleos sources …`: the MCP servers a deployment runs.
//!
//! Adding one connects it, asks what it advertises, and publishes a new tool generation — so a server
//! that answers `200` and contributes nothing cannot happen: the connection is made before the request
//! is acknowledged.

use crate::remote::{Overrides, Remote};
use nucleos::error::Result;
use nucleos::wire::api::{SourceListResponse, SourceRequest, SourceView};

/// Add or replace a source.
pub fn add(overrides: &Overrides, request: &SourceRequest) -> Result<String> {
    let body = serde_json::to_vec(request).map_err(|error| {
        nucleos::Error::invalid(format!("the source definition is not encodable: {error}"))
    })?;
    let text = Remote::open(overrides)?.call("POST", "/v1/sources", &body)?;
    let view: SourceView = serde_json::from_str(&text).map_err(|error| {
        nucleos::Error::internal(format!("the daemon's answer is unreadable: {error}"))
    })?;
    Ok(format!(
        "{}: {} tool(s) — {}",
        view.name,
        view.tools.len(),
        view.tools.join(", ")
    ))
}

/// List what runs, and what each contributes.
pub fn list(overrides: &Overrides) -> Result<String> {
    let text = Remote::open(overrides)?.call("GET", "/v1/sources", b"")?;
    let list: SourceListResponse = serde_json::from_str(&text).map_err(|error| {
        nucleos::Error::internal(format!("the daemon's source list is unreadable: {error}"))
    })?;
    if list.sources.is_empty() {
        return Ok("no sources: this deployment runs no tools".to_owned());
    }
    let mut out = String::new();
    for source in list.sources {
        out.push_str(&format!(
            "{:<10} {:<28} {}\n",
            source.name,
            source.command,
            source.tools.join(", ")
        ));
    }
    Ok(out.trim_end().to_owned())
}

/// Remove one.
pub fn remove(overrides: &Overrides, name: &str) -> Result<String> {
    Remote::open(overrides)?.call("DELETE", &format!("/v1/sources/{name}"), b"")
}
