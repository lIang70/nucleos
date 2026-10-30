//! Turning a connection into a principal, as far as this daemon can honestly go.
//!
//! Two layers, and the first one is the operating system's:
//!
//! - **The socket is private.** It lives in a directory only the owner can enter, and the socket
//!   file itself is `0600`. "Who may connect" is then the kernel's answer rather than ours, which is
//!   the only claim a unix-socket daemon can make without peer credentials. Reaching for
//!   `LOCAL_PEERCRED` would mean `unsafe`, which this workspace forbids — so the check stays where it
//!   can be enforced instead of where it would be nice to have.
//! - **An optional bearer token.** `NUCLEOS_TOKEN` turns it on. Every request then needs
//!   `Authorization: Bearer <token>`, including `describe` — a surface exempted from authentication
//!   is a surface that leaks what the deployment has.
//!
//! What this deliberately is not: multi-tenant identity. One token means one principal, taken from
//! `NUCLEOS_PRINCIPAL`, and every session is attributed to it. Deriving a principal *per token* is a
//! small change to this file and a large one to the model — the kernel keys sessions by
//! `(tenant, session)`, so tenants would need to stop sharing a token.

use nucleos::contract::ErrorCode;
use nucleos::error::Error;
use nucleos::wire::api::ErrorBody;

/// The tokens this daemon accepts, and who each of them is.
///
/// One token is one principal. A caller that presents none — or the wrong one — is refused; a caller
/// with no token *configured* is the single-tenant case, where the socket's permissions are the whole
/// authentication and `fallback` is the identity everything gets.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    entries: Vec<(String, nucleos::contract::Principal)>,
    fallback: nucleos::contract::Principal,
}

/// The document a deployment ships.
#[derive(Debug, serde::Deserialize)]
struct Document {
    tokens: Vec<Entry>,
}

#[derive(Debug, serde::Deserialize)]
struct Entry {
    token: String,
    tenant: String,
    subject: String,
}

impl Tokens {
    /// No tokens: the socket's permissions, one identity.
    #[must_use]
    pub fn confined_to(fallback: nucleos::contract::Principal) -> Self {
        Self {
            entries: Vec::new(),
            fallback,
        }
    }

    /// One token, one principal. The single-tenant deployment that wants a token as well as a
    /// private socket.
    #[must_use]
    pub fn single(token: String, principal: nucleos::contract::Principal) -> Self {
        Self {
            entries: vec![(token, principal)],
            fallback: nucleos::contract::Principal::default(),
        }
    }

    /// Read a token file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|error| {
            Error::invalid(format!("tokens {} are unreadable: {error}", path.display()))
        })?;
        let document: Document = serde_json::from_str(&text).map_err(|error| {
            Error::invalid(format!("tokens {} are not valid: {error}", path.display()))
        })?;
        let mut entries = Vec::with_capacity(document.tokens.len());
        for entry in document.tokens {
            if entry.token.is_empty() {
                return Err(Error::invalid("a token entry has an empty token"));
            }
            if entry.tenant.is_empty() {
                return Err(Error::invalid(format!(
                    "token for subject {:?} has no tenant; the kernel keys sessions by it",
                    entry.subject
                )));
            }
            entries.push((
                entry.token,
                nucleos::contract::Principal {
                    tenant: entry.tenant,
                    subject: entry.subject,
                },
            ));
        }
        Ok(Self {
            entries,
            fallback: nucleos::contract::Principal::default(),
        })
    }

    /// Whether any token is configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Who is calling, or why they may not.
    pub fn authenticate(&self, header: Option<&str>) -> Result<nucleos::contract::Principal, Error> {
        if self.entries.is_empty() {
            return Ok(self.fallback.clone());
        }
        let presented = header
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or_else(|| {
                Error::denied("this daemon requires a bearer token: send `Authorization: Bearer <token>`")
            })?;

        // Every entry is compared, and the match is selected afterwards: returning at the first hit
        // would let a caller learn how many tokens the deployment has by timing one request.
        let mut found: Option<&nucleos::contract::Principal> = None;
        for (token, principal) in &self.entries {
            let matched = constant_time_eq(presented.as_bytes(), token.as_bytes());
            if matched {
                found = Some(principal);
            }
        }
        found.cloned().ok_or_else(|| {
            Error::denied("the bearer token is not valid")
        })
    }
}

/// Whether a request may proceed.
///
/// `expected == None` means no token is configured: a daemon on a private socket, where the
/// operating system already answered this question.
pub fn authorize(expected: Option<&str>, header: Option<&str>) -> Result<(), Error> {
    let Some(expected) = expected else {
        return Ok(());
    };

    let presented = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(presented) = presented else {
        return Err(Error::denied(
            "this daemon requires a bearer token: send `Authorization: Bearer <token>`",
        ));
    };

    // Compared without an early exit. A byte-by-byte comparison that returns at the first
    // difference tells an attacker how much of the token was right, one request at a time.
    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err(Error::denied("the bearer token is not valid"));
    }
    Ok(())
}

/// Compare two byte strings without short-circuiting on the first difference.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Lengths leak either way, which is fine: a token's length is not the secret.
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The body a refused request gets.
///
/// The same shape as every other failure, so a client parses one error type rather than two.
#[must_use]
pub fn refusal(error: &Error) -> (u16, ErrorBody) {
    (
        401,
        ErrorBody {
            code: ErrorCode::Denied,
            reason: error.reason().to_owned(),
            retryable: false,
            retry_after_millis: None,
            details: std::collections::BTreeMap::new(),
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn no_token_configured_means_the_socket_is_the_authentication() {
        assert!(authorize(None, None).is_ok());
        assert!(authorize(None, Some("Bearer anything")).is_ok());
    }

    #[test]
    fn a_configured_token_is_required_and_checked() {
        assert!(authorize(Some("s3cret"), Some("Bearer s3cret")).is_ok());
        assert!(authorize(Some("s3cret"), None).is_err());
        assert!(
            authorize(Some("s3cret"), Some("s3cret")).is_err(),
            "no scheme"
        );
        assert!(authorize(Some("s3cret"), Some("Bearer s3cretx")).is_err());
        assert!(
            authorize(Some("s3cret"), Some("bearer s3cret")).is_err(),
            "the scheme is case-sensitive, as the spec says"
        );
        assert!(authorize(Some("s3cret"), Some("Basic s3cret")).is_err());
    }

    #[test]
    fn comparison_is_length_aware_and_exact() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
