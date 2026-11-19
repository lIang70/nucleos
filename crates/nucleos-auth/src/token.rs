//! Tenant tokens: issued once, stored hashed, verified per request.
//!
//! A token is not a password, and treating it like one would break the daemon. The distinction is
//! entropy:
//!
//! | | recovery password | tenant token |
//! | --- | --- | --- |
//! | entropy | chosen by a person, possibly weak | **256 random bits** |
//! | verified | once in a while | every request |
//! | correct hash | Argon2id, deliberately slow | **SHA-256, deliberately fast** |
//!
//! A slow hash protects a low-entropy input. Against 2^256 there is nothing to protect: an attacker
//! with the hash has no search space to run. What a slow hash *would* do here is add a second or more
//! to every request, which is the whole reason this is not Argon2.
//!
//! Two identifiers, kept apart on purpose: the **id** is what `nucleos tokens list` shows and what a
//! revoke names, and it says nothing about the token. Listing access must not leak a prefix of the
//! credential — a truncated token is a token somebody can brute-force.

use nucleos::error::{Error, Result};

/// The prefix, so a leaked string is recognisable as one of ours in a log or a repository scan.
pub const PREFIX: &str = "nu_";

/// A freshly issued token. Shown once, stored as [`hash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The id used by `list` and `remove`.
    pub id: String,
    /// The token itself. The only time it exists outside a client's memory.
    pub token: String,
    /// What the store keeps instead of the token.
    pub hash: String,
}

/// Issue one.
#[must_use]
pub fn issue() -> Issued {
    use base64::Engine as _;
    use rand_core::RngCore as _;

    let mut secret = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut secret);
    let token = format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)
    );

    let mut id_bytes = [0u8; 4];
    rand_core::OsRng.fill_bytes(&mut id_bytes);
    let id: String = id_bytes.iter().map(|byte| format!("{byte:02x}")).collect();

    Issued {
        hash: hash(&token),
        id,
        token,
    }
}

/// The stored form of a token.
///
/// SHA-256, hex. No salt: a salt stops one precomputation from covering many passwords, and there is
/// no precomputation over 256 random bits to stop.
#[must_use]
pub fn hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The token from an `Authorization: Bearer …` header.
pub fn from_header(value: &str) -> Result<&str> {
    value
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            Error::denied("this daemon needs a bearer token: send `Authorization: Bearer <token>`")
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn two_tokens_never_collide_and_the_id_does_not_contain_one() {
        let first = issue();
        let second = issue();
        assert_ne!(first.token, second.token);
        assert_ne!(first.id, second.id);
        assert!(first.token.starts_with(PREFIX));
        // The id is what `list` shows; a prefix of the token there would be a prefix of the credential.
        assert!(!first.token.contains(&first.id), "{first:?}");
        assert_eq!(first.id.len(), 8);
    }

    #[test]
    fn the_hash_is_the_stored_form_and_is_not_the_token() {
        let issued = issue();
        assert_eq!(hash(&issued.token), issued.hash);
        assert_ne!(issued.hash, issued.token);
        assert_eq!(issued.hash.len(), 64, "sha256 in hex");
        assert!(
            !issued.hash.contains(PREFIX),
            "the prefix would give it away"
        );
    }

    #[test]
    fn a_header_must_say_bearer() {
        assert_eq!(from_header("Bearer nu_abc").unwrap(), "nu_abc");
        for bad in ["nu_abc", "Basic nu_abc", "Bearer ", "bearer nu_abc"] {
            assert!(from_header(bad).is_err(), "{bad}");
        }
    }
}
