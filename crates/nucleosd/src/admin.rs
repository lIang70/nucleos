//! Verifying an administrator's signature, and the one endpoint that needs it.
//!
//! Configuration is a privilege, so the surface that changes it is signed rather than tokened: a
//! signature proves *who* with a key the daemon cannot forge, and the daemon holds only public halves.
//! That is the property §6.1 of `docs/config.md` turns on — a copy of the system store grants nobody
//! the ability to administer.
//!
//! Three checks, in this order, and each one is a test:
//!
//! 1. **Freshness** — the timestamp is within [`WINDOW_MILLIS`]. Wider is a longer window for a
//!    captured request; narrower breaks a machine whose clock is a minute off.
//! 2. **First sight of the nonce** — a second request carrying one already seen is refused, even if
//!    it is otherwise valid. Freshness alone would let a captured request be replayed within the
//!    window, which for `PUT /v1/policy` is enough to be a problem.
//! 3. **The signature**, over the canonical form built by the same function the CLI used. A verifier
//!    that rebuilt the string differently would be a second definition of what was signed.
//!
//! Failure says "denied" and nothing more specific to an unauthenticated caller: `unknown key` and
//! `bad signature` are the same answer from outside.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use nucleos::adapter::Clock;
use nucleos::contract::Principal;
use nucleos::error::{Error, Result};
use nucleos_auth::{PublicKey, canonical, parse_header};
use nucleos_sqlite::SystemStore;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// How far a request's timestamp may be from this machine's clock.
///
/// Sixty seconds: long enough for clock skew between two machines somebody set up by hand, short
/// enough that a captured request is worth almost nothing.
pub const WINDOW_MILLIS: u64 = 60_000;

/// The most nonces kept. Beyond the window they are forgotten anyway; this bounds the memory of a
/// daemon somebody is posting at.
const NONCE_CAP: usize = 8192;

/// Where administrator keys are filed. The same key `nucleos init` writes.
pub const ADMIN_KEYS_KEY: &str = "auth/admin_keys";

/// Checks a request against the keys a deployment enrolled.
pub struct Verifier {
    system: Arc<SystemStore>,
    clock: Arc<dyn Clock>,
    /// Nonce → when it was seen, so the window can prune it.
    seen: Mutex<BTreeMap<String, u64>>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The clock is a trait object with no useful representation; the keys are in the store.
        f.debug_struct("Verifier").finish_non_exhaustive()
    }
}

impl Verifier {
    /// A verifier over the system store.
    #[must_use]
    pub fn new(system: Arc<SystemStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            system,
            clock,
            seen: Mutex::new(BTreeMap::new()),
        }
    }

    /// Check one request and answer who made it.
    pub async fn verify(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        header: Option<&str>,
    ) -> Result<Principal> {
        let header = header.ok_or_else(|| {
            Error::denied(format!(
                "this endpoint needs an administrative signature: `Authorization: {} …`",
                nucleos_auth::SCHEME
            ))
        })?;
        let signed = parse_header(header)?;

        let now = self.clock.now_unix_millis();
        let skew = now.abs_diff(signed.at_unix_millis);
        if skew > WINDOW_MILLIS {
            tracing::warn!(
                event = "admin_signature_stale",
                key_id = %signed.key_id,
                skew_millis = skew,
            );
            return Err(Error::denied(format!(
                "the request is {skew} ms from this machine's clock, and the window is {WINDOW_MILLIS}"
            )));
        }

        // A nonce already seen is a replay, whatever else is true of it.
        {
            let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            seen.retain(|_, at| now.abs_diff(*at) <= WINDOW_MILLIS);
            if seen.contains_key(&signed.nonce) {
                tracing::warn!(event = "admin_signature_replayed", key_id = %signed.key_id);
                return Err(Error::denied("this request has already been made"));
            }
            if seen.len() >= NONCE_CAP {
                // Oldest first: they are the closest to expiring anyway.
                if let Some(oldest) = seen.keys().next().cloned() {
                    seen.remove(&oldest);
                }
            }
            seen.insert(signed.nonce.clone(), signed.at_unix_millis);
        }

        // Read the keys on every request rather than caching them: revocation has to be immediate, and
        // a key that keeps working for the length of a cache is a key somebody revoked.
        let keys = self.keys().await?;
        let Some(key) = keys.iter().find(|key| key.id == signed.key_id) else {
            tracing::warn!(event = "admin_key_unknown", key_id = %signed.key_id);
            return Err(Error::denied("this request is not authorised"));
        };

        let text = canonical(
            method,
            path,
            body,
            signed.at_unix_millis,
            &signed.nonce,
        );
        if !key.verifies(text.as_bytes(), &signed.signature)? {
            tracing::warn!(event = "admin_signature_invalid", key_id = %key.id);
            return Err(Error::denied("this request is not authorised"));
        }

        let principal = key.principal();
        tracing::info!(
            event = "admin_request",
            method,
            path,
            key_id = %key.id,
            tenant = %principal.tenant,
            subject = %principal.subject,
        );
        Ok(principal)
    }

    async fn keys(&self) -> Result<Vec<PublicKey>> {
        let Some(raw) = self.system.get(ADMIN_KEYS_KEY)? else {
            return Err(Error::denied(
                "no administrator key is enrolled; run `nucleos init`",
            ));
        };
        serde_json::from_str(&raw)
            .map_err(|error| Error::internal(format!("stored admin keys are unreadable: {error}")))
    }
}

/// An authenticated administrator, with the body the signature covered.
///
/// The body comes with the principal because the signature covers it: an extractor that verified a
/// hash of one body and handed the handler another would be checking the wrong bytes.
#[derive(Debug)]
pub struct Admin {
    /// Who signed it.
    pub principal: Principal,
    /// What they signed.
    pub body: Bytes,
}

impl FromRequest<Arc<crate::http::App>> for Admin {
    type Rejection = crate::http::ApiError;

    async fn from_request(
        request: Request,
        state: &Arc<crate::http::App>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 8 * 1024 * 1024)
            .await
            .map_err(|error| {
                crate::http::ApiError::from_error(&Error::invalid(format!(
                    "cannot read the request body: {error}"
                )))
            })?;

        let verifier = state.admins().ok_or_else(|| {
            crate::http::ApiError::from_error(&Error::unavailable(
                "this daemon has no system store, so no administrator key is enrolled",
            ))
        })?;

        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let principal = verifier
            .verify(
                parts.method.as_str(),
                parts.uri.path(),
                &bytes,
                header,
            )
            .await
            .map_err(|error| crate::http::ApiError::unauthorized(&error))?;

        Ok(Self {
            principal,
            body: bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use nucleos_testkit::FixedClock;

    async fn verifier_with(key: &nucleos_auth::KeyPair) -> Verifier {
        let system = Arc::new(SystemStore::in_memory().unwrap());
        let public = key.public("acme", "alice");
        system
            .set(ADMIN_KEYS_KEY, &serde_json::to_string(&vec![public]).unwrap())
            .await
            .unwrap();
        Verifier::new(system, Arc::new(FixedClock::at(1_000_000)))
    }

    #[tokio::test]
    async fn a_signed_request_is_accepted_and_named() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        let header = key.sign_request(
            &key.public("acme", "alice").id,
            "GET",
            "/v1/config",
            b"",
            1_000_000,
        );

        let principal = verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap();
        assert_eq!(principal.tenant, "acme");
        assert_eq!(principal.subject, "alice");
    }

    #[tokio::test]
    async fn an_unsigned_request_is_refused_with_the_scheme() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        let error = verifier.verify("GET", "/v1/config", b"", None).await.unwrap_err();
        assert!(error.reason().contains("Nucleos-Admin"), "{error:?}");
    }

    #[tokio::test]
    async fn a_stale_request_is_refused() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        // Signed for a moment two minutes ago: outside the window in either direction.
        let header = key.sign_request("x", "GET", "/v1/config", b"", 1_000_000 - 120_000);
        let header = header.replace(":x:", &format!(":{}:", key.public("acme", "alice").id));
        let error = verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap_err();
        assert!(error.reason().contains("window"), "{error:?}");
    }

    #[tokio::test]
    async fn the_same_request_twice_is_refused_the_second_time() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        let header = key.sign_request(
            &key.public("acme", "alice").id,
            "GET",
            "/v1/config",
            b"",
            1_000_000,
        );

        verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap();
        // Freshness alone would have allowed this: it is inside the window and correctly signed.
        let error = verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap_err();
        assert!(error.reason().contains("already been made"), "{error:?}");
    }

    #[tokio::test]
    async fn a_signature_for_one_request_does_not_authorise_another() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        let header = key.sign_request(
            &key.public("acme", "alice").id,
            "GET",
            "/v1/config",
            b"",
            1_000_000,
        );

        // Same signature, different path: the canonical form covers the path, so this cannot pass.
        let error = verifier
            .verify("PUT", "/v1/policy", b"{}", Some(&header))
            .await
            .unwrap_err();
        assert!(error.reason().contains("not authorised"), "{error:?}");
    }

    #[tokio::test]
    async fn an_unknown_key_id_is_refused_without_saying_which_keys_exist() {
        let key = nucleos_auth::KeyPair::generate();
        let verifier = verifier_with(&key).await;
        let header = key.sign_request("0000dead", "GET", "/v1/config", b"", 1_000_000);
        let error = verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap_err();
        assert_eq!(error.reason(), "this request is not authorised");
    }

    #[tokio::test]
    async fn a_deployment_with_no_keys_says_to_initialise() {
        let system = Arc::new(SystemStore::in_memory().unwrap());
        let verifier = Verifier::new(system, Arc::new(FixedClock::at(1_000_000)));
        let key = nucleos_auth::KeyPair::generate();
        let header = key.sign_request("x", "GET", "/v1/config", b"", 1_000_000);
        let error = verifier
            .verify("GET", "/v1/config", b"", Some(&header))
            .await
            .unwrap_err();
        assert!(error.reason().contains("nucleos init"), "{error:?}");
    }
}
