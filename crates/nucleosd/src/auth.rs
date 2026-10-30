//! Turning a connection into a principal, as far as this daemon can honestly go.
//!
//! Three layers, and each answers a different question:
//!
//! | Question | Answered by |
//! | --- | --- |
//! | may this host user talk to the daemon at all | the socket's mode and its directory's mode |
//! | which tenant is this | a bearer token, hashed, looked up in the system store |
//! | may this change configuration | an Ed25519 signature, checked against an enrolled public key |
//!
//! This module is the middle row. It reads the store on every request rather than caching: revocation
//! has to be immediate, and a token that keeps working for the length of a cache is a token somebody
//! revoked. A deployment with **no** tokens configured is the single-identity case — the socket's
//! permissions are then the whole authentication, and every call is attributed to the fallback
//! principal — which is a state the store answers, not a variable somebody sets.
//!
//! What this deliberately is not: multi-tenant *administration*. One token is one tenant identity;
//! deciding what that tenant may do is the policy's job, and deciding whether a caller may reconfigure
//! the deployment is the signature's.

use nucleos::contract::{ErrorCode, Principal};
use nucleos::wire::api::ErrorBody;
use nucleos::error::Error;
use nucleos_auth::token;
use nucleos_sqlite::SystemStore;
use std::sync::Arc;

/// The tokens a deployment has issued, and the identity everything falls back to when it has none.
#[derive(Debug, Clone)]
pub struct TokenAuth {
    system: Arc<SystemStore>,
    fallback: Principal,
}

impl TokenAuth {
    /// Authenticate against a store, falling back to one principal when it holds no tokens.
    #[must_use]
    pub fn new(system: Arc<SystemStore>, fallback: Principal) -> Self {
        Self { system, fallback }
    }

    /// Who is calling, or why they may not.
    pub fn authenticate(&self, header: Option<&str>) -> Result<Principal, Error> {
        let required = self
            .system
            .tokens_required()
            .map_err(|error| Error::internal(format!("cannot read the token table: {error}")))?;
        if !required {
            // No token exists, so possessing one cannot be the check. The socket's permissions are.
            return Ok(self.fallback.clone());
        }

        let presented = header
            .map(token::from_header)
            .transpose()?
            .ok_or_else(|| {
                Error::denied("this daemon needs a bearer token: send `Authorization: Bearer <token>`")
            })?;

        // The lookup is by hash, so a wrong token and an unknown token are one comparison — there is
        // no per-entry scan whose timing would leak which tokens exist.
        let found = self
            .system
            .tenant_for_hash(&token::hash(presented))
            .map_err(|error| Error::internal(format!("cannot read the token table: {error}")))?;
        match found {
            Some((_id, tenant, subject)) => Ok(Principal { tenant, subject }),
            None => Err(Error::denied("the bearer token is not valid")),
        }
    }
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
    use nucleos_auth::Issued;

    fn auth() -> (TokenAuth, Arc<SystemStore>) {
        let system = Arc::new(SystemStore::in_memory().unwrap());
        let auth = TokenAuth::new(
            Arc::clone(&system),
            Principal {
                tenant: "local".to_owned(),
                subject: "socket".to_owned(),
            },
        );
        (auth, system)
    }

    async fn issue(system: &Arc<SystemStore>, tenant: &str, subject: &str) -> Issued {
        // Mirrors what the endpoint does: issuing a token is what makes tokens required, and the
        // flag is what keeps a deployment from reopening when its last token is revoked.
        system.require_tokens().await.unwrap();
        let issued = token::issue();
        system
            .add_token(&nucleos_sqlite::IssuedToken {
                id: issued.id.clone(),
                tenant: tenant.to_owned(),
                subject: subject.to_owned(),
                hash: issued.hash.clone(),
                created_unix_millis: 1,
            })
            .await
            .unwrap();
        issued
    }

    #[tokio::test]
    async fn with_no_tokens_the_socket_is_the_authentication() {
        let (auth, _) = auth();
        let principal = auth.authenticate(None).unwrap();
        assert_eq!(principal.tenant, "local");
    }

    #[tokio::test]
    async fn a_known_token_names_its_tenant() {
        let (auth, system) = auth();
        let issued = issue(&system, "acme", "alice").await;

        let principal = auth
            .authenticate(Some(&format!("Bearer {}", issued.token)))
            .unwrap();
        assert_eq!(principal.tenant, "acme");
        assert_eq!(principal.subject, "alice");
    }

    #[tokio::test]
    async fn once_a_token_exists_the_socket_is_no_longer_enough() {
        // The failure this prevents: a deployment that issues tokens and still lets an unauthenticated
        // local caller in, because "no header" used to mean the fallback.
        let (auth, system) = auth();
        issue(&system, "acme", "alice").await;
        let error = auth.authenticate(None).unwrap_err();
        assert!(error.reason().contains("bearer"), "{error:?}");
    }

    #[tokio::test]
    async fn an_unknown_token_is_refused() {
        let (auth, system) = auth();
        issue(&system, "acme", "alice").await;
        let error = auth
            .authenticate(Some("Bearer nu_not-a-real-token"))
            .unwrap_err();
        assert!(error.reason().contains("not valid"), "{error:?}");
    }

    #[tokio::test]
    async fn revoking_a_token_takes_effect_on_the_next_request() {
        let (auth, system) = auth();
        let issued = issue(&system, "acme", "alice").await;
        let header = format!("Bearer {}", issued.token);
        assert!(auth.authenticate(Some(&header)).is_ok());

        assert!(system.revoke_token(&issued.id).await.unwrap());
        let error = auth.authenticate(Some(&header)).unwrap_err();
        assert!(error.reason().contains("not valid"), "{error:?}");
    }

    #[tokio::test]
    async fn revoking_the_last_token_does_not_reopen_the_door() {
        // The failure this prevents: an operator revokes a leaked token, the table empties, and the
        // daemon falls back to "no tokens means the socket is enough" — reopening to every local
        // caller at the moment somebody was trying to close it.
        let (auth, system) = auth();
        let issued = issue(&system, "acme", "alice").await;
        system.require_tokens().await.unwrap();
        system.revoke_token(&issued.id).await.unwrap();

        let error = auth.authenticate(None).unwrap_err();
        assert!(error.reason().contains("bearer"), "{error:?}");
        let error = auth
            .authenticate(Some(&format!("Bearer {}", issued.token)))
            .unwrap_err();
        assert!(error.reason().contains("not valid"), "{error:?}");
    }

    #[tokio::test]
    async fn the_store_never_holds_the_token_itself() {
        let (_, system) = auth();
        let issued = issue(&system, "acme", "alice").await;
        let listed = system.list_tokens().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].hash.is_empty(), "a listing carries no hash");
        // And the row that does carry one carries the hash, not the token.
        assert!(system
            .tenant_for_hash(&issued.hash)
            .unwrap()
            .is_some());
        assert!(system
            .tenant_for_hash(&issued.token)
            .unwrap()
            .is_none());
    }
}
