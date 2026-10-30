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

pub mod password;

pub use password::{Cost, Limiter, MIN_PASSWORD_LEN, PasswordHash};

/// The key under which a deployment's recovery password hash is kept.
///
/// A constant so that the writer and the reader cannot disagree about the spelling, which is the one
/// class of bug a key-value store is genuinely good at producing.
pub const RECOVERY_KEY: &str = "auth/recovery_password";
