//! Credentials for the daemon: the recovery password, and — later — the administrator's keys.
//!
//! A separate crate because none of this belongs in the kernel. The kernel asks a [`Policy`] whether
//! a call may happen and receives a principal; it does not hash passwords, verify signatures, or know
//! that a keychain exists. Keeping the credential handling outside means the kernel keeps compiling
//! without a crypto stack, and this crate can grow an algorithm without touching an ABI.
//!
//! What lives here is the *local* half of authentication:
//!
//! | Credential | Purpose | Where the secret is |
//! | --- | --- | --- |
//! | recovery password | enrol an administrator key, and re-enrol when all are lost | stretched with Argon2id |
//! | administrator public keys | authenticate a configuration change | the store — public by definition |
//!
//! The *remote* half — bearer tokens for tenant callers, verified on every request — stays in the
//! daemon, because it is a transport concern: a token is checked at the edge, a signature will be
//! checked at the same edge, and neither should reach the kernel.

pub mod key;
pub mod signature;
pub mod token;
pub mod keychain;
pub mod password;

pub use key::{KeyPair, PublicKey};
pub use signature::{canonical, parse_header, Signed, SCHEME};
pub use token::Issued;
pub use keychain::Store as KeyStore;
pub use password::{Cost, Limiter, MIN_PASSWORD_LEN, PasswordHash};

/// The key under which a deployment's recovery password hash is kept.
///
/// A constant so that the writer and the reader cannot disagree about the spelling, which is the one
/// class of bug a key-value store is genuinely good at producing.
pub const RECOVERY_KEY: &str = "auth/recovery_password";

/// Sixteen random bytes, hex-encoded: the one-time half of a signed request.
///
/// Not a UUID and not a counter. A counter would need shared state to start from; a UUID is 122 bits of
/// randomness behind a format that implies meaning it does not have. Sixteen bytes within a ±60 s
/// window is a collision probability nobody will meet.
pub(crate) fn nonce() -> String {
    let mut bytes = [0u8; 16];
    use rand_core::RngCore as _;
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
