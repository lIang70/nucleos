//! What an administrator signs, and how the signature travels.
//!
//! One function builds the string, one builds the header, one parses it — and the daemon calls the
//! same builders as the CLI. That is the whole safety argument: "what exactly is signed" is a question
//! with one answer in this workspace, not a convention every call site re-derives.
//!
//! ```text
//! nucleos-admin-v1\n
//! <METHOD>\n
//! <PATH>\n
//! <sha256(body) as lowercase hex>\n
//! <unix milliseconds>\n
//! <nonce>
//! ```
//!
//! Every line is there for a reason a missing line would undo:
//!
//! - **the version prefix** stops a signature made for one canonical form from being valid under a
//!   later one;
//! - **the method and path** stop a signature captured on a read from being replayed against a write;
//! - **the body hash** stops a captured signature from carrying a different body;
//! - **the timestamp and nonce** bound how long a captured request is worth anything, and let the
//!   daemon refuse a second copy of one it has already seen.
//!
//! The header carries the same timestamp and nonce, because the daemon has to rebuild the string in
//! order to check it — and a signature over fields the verifier has to guess is not a signature.

use crate::key::KeyPair;
use nucleos::error::{Error, Result};

/// The scheme, and its version. Changing the canonical form means changing both.
pub const SCHEME: &str = "Nucleos-Admin";
/// The prefix of every canonical string.
pub const CANONICAL_PREFIX: &str = "nucleos-admin-v1";

/// One signed request, as it travelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// Which key claims to have made it.
    pub key_id: String,
    /// When, in unix milliseconds.
    pub at_unix_millis: u64,
    /// The one-time value that makes it not replayable.
    pub nonce: String,
    /// The signature itself, base64.
    pub signature: String,
}

/// The bytes that were signed.
#[must_use]
pub fn canonical(
    method: &str,
    path: &str,
    body: &[u8],
    at_unix_millis: u64,
    nonce: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{CANONICAL_PREFIX}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        hex,
        at_unix_millis,
        nonce
    )
}

impl KeyPair {
    /// Sign a request and produce the header value to send.
    ///
    /// The nonce comes from the operating system's randomness: it only has to be unique within the
    /// replay window, not globally, because the daemon forgets it after that.
    #[must_use]
    pub fn sign_request(
        &self,
        key_id: &str,
        method: &str,
        path: &str,
        body: &[u8],
        at_unix_millis: u64,
    ) -> String {
        let nonce = crate::nonce();
        let signature = self.sign(canonical(method, path, body, at_unix_millis, &nonce).as_bytes());
        format!("{SCHEME} {key_id}:{at_unix_millis}:{nonce}:{signature}")
    }
}

/// Read a header value back.
pub fn parse_header(value: &str) -> Result<Signed> {
    let rest = value
        .strip_prefix(SCHEME)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or_else(|| {
            Error::denied(format!(
                "expected `{SCHEME} <key>:<millis>:<nonce>:<signature>`"
            ))
        })?;

    let parts: Vec<&str> = rest.split(':').collect();
    let [key_id, at, nonce, signature] = parts.as_slice() else {
        return Err(Error::denied(
            "the administrative signature has four parts: key id, timestamp, nonce, signature",
        ));
    };
    let at_unix_millis = at
        .parse::<u64>()
        .map_err(|_| Error::denied(format!("{at:?} is not a timestamp in unix milliseconds")))?;
    if key_id.is_empty() || nonce.is_empty() || signature.is_empty() {
        return Err(Error::denied(
            "the administrative signature has an empty part",
        ));
    }
    Ok(Signed {
        key_id: (*key_id).to_owned(),
        at_unix_millis,
        nonce: (*nonce).to_owned(),
        signature: (*signature).to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::key::KeyPair;

    #[test]
    fn the_canonical_form_names_every_part_that_matters() {
        let text = canonical(
            "post",
            "/v1/models",
            b"{\"name\":\"primary\"}",
            1_700_000_000_000,
            "abc",
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CANONICAL_PREFIX);
        assert_eq!(lines[1], "POST", "the method is normalised");
        assert_eq!(lines[2], "/v1/models");
        // Computed again here, from the outside: the canonical form has to be checkable by anybody
        // implementing the other side, not only by the function that produced it.
        use sha2::{Digest, Sha256};
        let expected: String = Sha256::digest(b"{\"name\":\"primary\"}")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(lines[3], expected);
        assert_eq!(lines[4], "1700000000000");
        assert_eq!(lines[5], "abc");
    }

    #[test]
    fn a_body_change_changes_the_canonical_string() {
        let a = canonical("POST", "/v1/models", b"one", 1, "n");
        let b = canonical("POST", "/v1/models", b"two", 1, "n");
        assert_ne!(a, b, "a captured signature must not carry another body");
    }

    #[test]
    fn a_method_change_changes_it_too() {
        // The failure this prevents: a signature captured on a read replayed against a write.
        assert_ne!(
            canonical("GET", "/v1/config", b"", 1, "n"),
            canonical("PUT", "/v1/config", b"", 1, "n")
        );
    }

    #[test]
    fn a_header_round_trips() {
        let key = KeyPair::generate();
        let public = key.public("acme", "alice");
        let header = key.sign_request(&public.id, "GET", "/v1/config", b"", 1_700_000_000_000);
        let signed = parse_header(&header).unwrap();
        assert_eq!(signed.key_id, public.id);
        assert_eq!(signed.at_unix_millis, 1_700_000_000_000);

        let canonical = canonical(
            "GET",
            "/v1/config",
            b"",
            signed.at_unix_millis,
            &signed.nonce,
        );
        assert!(
            public
                .verifies(canonical.as_bytes(), &signed.signature)
                .unwrap()
        );
    }

    #[test]
    fn two_signings_of_one_request_differ_by_their_nonce() {
        // Without a nonce, two identical requests would carry identical signatures, and the daemon
        // could not tell a replay from a repeat.
        let key = KeyPair::generate();
        let first = key.sign_request("k", "GET", "/v1/config", b"", 1);
        let second = key.sign_request("k", "GET", "/v1/config", b"", 1);
        assert_ne!(first, second);
    }

    #[test]
    fn a_malformed_header_is_refused_with_the_shape_it_wanted() {
        for bad in [
            "Bearer x",
            "Nucleos-Admin",
            "Nucleos-Admin a:b:c",
            "Nucleos-Admin a:x:c:d",
        ] {
            let error = parse_header(bad).unwrap_err();
            assert!(!error.reason().is_empty(), "{bad}");
        }
        let error = parse_header("Nucleos-Admin a:x:c:d").unwrap_err();
        assert!(error.reason().contains("timestamp"), "{error:?}");
    }
}
