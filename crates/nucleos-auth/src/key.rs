//! The administrator's key: Ed25519, generated locally, with the private half never leaving the machine.
//!
//! This is the reason the design has no generated shared secret. A daemon that holds a signing key can
//! be made to mint administration by anyone who copies its database; a daemon that holds only public
//! keys cannot. So the keypair is created where the administrator is, the private half goes to the
//! operating system's keychain, and the daemon is told the public half and nothing else.
//!
//! What this module owns is the material: generation, encoding, a stable identifier, and signing for
//! the half of it that lives on the administrator's side. Verifying a *request* is the daemon's job
//! and arrives with the administrative ABI — the format questions it has to answer (what exactly is
//! signed) belong with the transport that carries the signature, not here.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use nucleos::error::{Error, Result};

/// A keypair, in memory. The private half is dropped when this does.
///
/// No `Debug`, deliberately: a `Debug` impl on a private key is a private key in somebody's log line,
/// and the derive is one line away from every `dbg!` and every `{:?}` in a panic message.
pub struct KeyPair {
    signing: SigningKey,
}

/// The public half, as the daemon stores it.
///
/// Serde because the system store keeps it as a row: a public key is not a secret, so encoding it in
/// the clear is the point rather than an oversight.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicKey {
    /// What the administrator is called in the record.
    pub id: String,
    /// Base64 of the 32-byte verifying key.
    pub key: String,
    /// The tenant this key acts for.
    ///
    /// A key *is* an identity, so it carries the whole one: without the tenant, a verified signature
    /// would produce a principal that cannot be scoped, and every configuration change would have to
    /// arrive with its own tenant claim — a second thing to verify and a second thing to get wrong.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
}

impl PublicKey {
    /// A key from the bytes somebody handed over, as a hex-free base64 string.
    ///
    /// The id is derived here rather than accepted from the caller: an id that a submitter chooses is
    /// an id that can collide with an existing administrator's.
    pub fn from_public(
        key: &str,
        tenant: impl Into<String>,
        subject: impl Into<String>,
    ) -> Result<Self> {
        let bytes = base64_decode(key)?;
        let length = bytes.len();
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::invalid(format!("a public key is 32 bytes; this is {length}")))?;
        // Parsed, not just length-checked: a 32-byte string that is not a curve point would fail
        // every verification later, with a message about the signature rather than the key.
        VerifyingKey::from_bytes(&array).map_err(|error| {
            Error::invalid(format!("that is not an Ed25519 public key: {error}"))
        })?;
        Ok(Self {
            id: fingerprint(&array),
            key: base64_encode(&array),
            tenant: tenant.into(),
            subject: subject.into(),
        })
    }

    /// The principal this key authenticates.
    #[must_use]
    pub fn principal(&self) -> nucleos::contract::Principal {
        nucleos::contract::Principal {
            tenant: self.tenant.clone(),
            subject: self.subject.clone(),
        }
    }
}

impl KeyPair {
    /// Generate a new keypair from the operating system's randomness.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// Rebuild one from the bytes a keychain holds.
    pub fn from_secret(bytes: &[u8]) -> Result<Self> {
        let array: [u8; 32] = bytes.try_into().map_err(|_| {
            Error::invalid(format!(
                "an Ed25519 secret is 32 bytes; the keychain holds {}",
                bytes.len()
            ))
        })?;
        Ok(Self {
            signing: SigningKey::from_bytes(&array),
        })
    }

    /// The bytes to hand to the keychain.
    ///
    /// The seed, not the expanded key: 32 bytes is what `SigningKey::from_bytes` takes, and it is the
    /// smaller thing to keep secret.
    #[must_use]
    pub fn secret(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// The public half, ready to be stored.
    #[must_use]
    pub fn public(&self, tenant: impl Into<String>, subject: impl Into<String>) -> PublicKey {
        let verifying = self.signing.verifying_key();
        PublicKey {
            id: fingerprint(verifying.as_bytes()),
            key: base64_encode(verifying.as_bytes()),
            tenant: tenant.into(),
            subject: subject.into(),
        }
    }

    /// Sign a canonical request string.
    ///
    /// The canonical form is the transport's business — this signs bytes and says so, so that
    /// "what exactly did we sign" is answered in one place when the administrative ABI defines it,
    /// instead of being implied by every call site.
    #[must_use]
    pub fn sign(&self, canonical: &[u8]) -> String {
        base64_encode(&self.signing.sign(canonical).to_bytes())
    }
}

impl PublicKey {
    /// Whether a signature over `canonical` was made by this key.
    pub fn verifies(&self, canonical: &[u8], signature: &str) -> Result<bool> {
        let bytes = base64_decode(&self.key)?;
        let verifying = VerifyingKey::from_bytes(
            &bytes
                .try_into()
                .map_err(|_| Error::invalid(format!("public key {} is not 32 bytes", self.id)))?,
        )
        .map_err(|error| Error::invalid(format!("public key {} is unusable: {error}", self.id)))?;

        let signature = Signature::from_slice(&base64_decode(signature)?)
            .map_err(|error| Error::invalid(format!("signature is unusable: {error}")))?;
        Ok(verifying.verify_strict(canonical, &signature).is_ok())
    }
}

/// The short, stable name of a key: the first four bytes, in hex.
///
/// Short because it is written in logs and in a refusal ("signed by 7f3a…"), and stable because it is
/// derived from the key rather than assigned — two administrators cannot end up with the same one by
/// accident, and a re-enrolment of the same key is recognisable.
#[must_use]
pub fn fingerprint(verifying: &[u8]) -> String {
    verifying
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|error| Error::invalid(format!("not valid base64: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_signature_only_verifies_under_the_key_that_made_it() {
        let mine = KeyPair::generate();
        let theirs = KeyPair::generate();
        let canonical = b"nucleos-admin-v1\nPOST\n/v1/models\n";

        let signature = mine.sign(canonical);
        assert!(
            mine.public("acme", "alice")
                .verifies(canonical, &signature)
                .unwrap()
        );
        // The property the whole design rests on: a signature is not transferable to another key.
        assert!(
            !theirs
                .public("acme", "mallory")
                .verifies(canonical, &signature)
                .unwrap()
        );
        // Nor to another request.
        assert!(
            !mine
                .public("acme", "alice")
                .verifies(b"nucleos-admin-v1\nPOST\n/v1/policy\n", &signature)
                .unwrap()
        );
    }

    #[test]
    fn a_key_round_trips_through_its_secret_bytes() {
        // The keychain holds bytes; a restart has to rebuild the same key from them.
        let original = KeyPair::generate();
        let restored = KeyPair::from_secret(&original.secret()).unwrap();
        assert_eq!(
            original.public("acme", "alice"),
            restored.public("acme", "alice")
        );
        assert_eq!(
            original.public("acme", "alice").key,
            restored.public("acme", "alice").key
        );
    }

    #[test]
    fn the_identifier_is_short_and_derived_from_the_key() {
        let key = KeyPair::generate().public("acme", "alice");
        assert_eq!(key.id.len(), 8, "{}", key.id);
        assert!(key.id.chars().all(|c| c.is_ascii_hexdigit()));

        // Same key, same name; different key, different name.
        let again = KeyPair::from_secret(&KeyPair::generate().secret())
            .unwrap()
            .public("acme", "alice");
        assert_ne!(key.id, again.id);
    }

    #[test]
    fn a_secret_of_the_wrong_length_is_refused_by_name() {
        let Err(error) = KeyPair::from_secret(&[0u8; 16]) else {
            panic!("16 bytes is not an Ed25519 secret");
        };
        assert!(error.reason().contains("32 bytes"), "{error:?}");
    }

    #[test]
    fn a_corrupted_public_key_is_reported_rather_than_silently_failing() {
        let key = PublicKey {
            id: "00000000".to_owned(),
            key: "not base64".to_owned(),
            tenant: "acme".to_owned(),
            subject: "alice".to_owned(),
        };
        assert!(key.verifies(b"x", "AAAA").is_err());
    }
}
