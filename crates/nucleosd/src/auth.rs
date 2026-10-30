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
