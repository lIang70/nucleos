//! `nucleos tokens …`: who may call this daemon, and as which tenant.
//!
//! The value is printed **once** — the store keeps only a hash, so a later `list` cannot hand it back.
//! That is the whole reason this command exists rather than an environment variable: an operator needs
//! a moment at which the credential is created, named and recorded, and a place to revoke it later
//! without a restart.

use crate::remote::{Overrides, Remote};
use nucleos::error::Result;
use nucleos::wire::api::{TokenIssueRequest, TokenIssued, TokenListResponse};

/// Issue a token and print it, once.
pub fn add(overrides: &Overrides, tenant: &str, subject: &str) -> Result<String> {
    let body = serde_json::to_vec(&TokenIssueRequest {
        tenant: tenant.to_owned(),
        subject: subject.to_owned(),
    })
    .map_err(|error| nucleos::Error::invalid(format!("cannot encode the request: {error}")))?;

    let text = Remote::open(overrides)?.call("POST", "/v1/tokens", &body)?;
    let issued: TokenIssued = serde_json::from_str(&text).map_err(|error| {
        nucleos::Error::internal(format!("the daemon's answer is unreadable: {error}"))
    })?;
    Ok(format!(
        "{}\n\nGive this to its client now: the store keeps only a hash, and no command can print it \
         again.\nid {}  tenant {}  subject {}",
        issued.token, issued.id, issued.tenant, issued.subject
    ))
}

/// List what exists, without values.
pub fn list(overrides: &Overrides) -> Result<String> {
    let text = Remote::open(overrides)?.call("GET", "/v1/tokens", b"")?;
    let list: TokenListResponse = serde_json::from_str(&text).map_err(|error| {
        nucleos::Error::internal(format!("the daemon's token list is unreadable: {error}"))
    })?;
    if list.tokens.is_empty() {
        return Ok("no tokens: this daemon trusts its socket's permissions".to_owned());
    }
    let mut out = format!("{:<10} {:<12} {:<12} {}\n", "id", "tenant", "subject", "issued");
    for token in list.tokens {
        out.push_str(&format!(
            "{:<10} {:<12} {:<12} {}\n",
            token.id, token.tenant, token.subject, token.created_unix_millis
        ));
    }
    Ok(out.trim_end().to_owned())
}

/// Revoke one.
pub fn remove(overrides: &Overrides, id: &str) -> Result<String> {
    Remote::open(overrides)?.call("DELETE", &format!("/v1/tokens/{id}"), b"")
}
