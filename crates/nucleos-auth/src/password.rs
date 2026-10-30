//! The credential a deployment keeps for itself: a recovery password.
//!
//! Why a password exists at all, in a design that prefers public keys: because "all the administrator
//! keys are lost" has to have an answer that is not "reinstall". It is a **bootstrap and recovery**
//! credential, never a request credential —
//!
//! - it is used to enrol an administrator's key at `init`, and to re-enrol one afterwards,
//! - it is verified only where the store file is already readable, so possessing it is necessary,
//! - it is stretched with Argon2id, so a copy of the hash is expensive to attack rather than
//!   immediately useful.
//!
//! What it is not: a second way to authenticate ordinary requests. A password on the wire is a shared
//! secret with an unbounded lifetime, and every request that carries one is a chance to capture it.
//!
//! ## What the rate limiter is and is not
//!
//! Recovery runs in a local process that already has the database file. Someone with that file can
//! attack the hash offline, at whatever rate their hardware allows — no limiter in this process can
//! reach that. The limiter that matters is Argon2's memory cost. [`Limiter`] exists for the case it
//! *can* address: a person at a keyboard guessing, in the one process that is running.

use argon2::password_hash::{PasswordHash as Phc, PasswordHasher, PasswordVerifier, SaltString, rand_core};
use argon2::{Algorithm, Argon2, Params, Version};
use nucleos::error::{Error, Result};
use std::time::{Duration, Instant};

/// How long a password has to be.
///
/// Not a strength meter: a length floor rejects `password` and `x`, and anything past that is the
/// operator's judgement. Twelve because it is the shortest length at which a human-chosen phrase is
/// plausibly not in a wordlist.
pub const MIN_PASSWORD_LEN: usize = 12;

/// How expensive hashing is. The parameters are encoded in the stored string, so raising them later
/// does not invalidate existing hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    /// Memory in kibibytes. This is the parameter that defeats parallel attack hardware.
    pub memory_kib: u32,
    /// Passes over that memory.
    pub passes: u32,
    /// Lanes, for parallelism.
    pub lanes: u32,
}

impl Cost {
    /// The deployment default: 64 MiB, three passes, four lanes.
    ///
    /// Deliberately above the crate's default (19 MiB, two passes). A local admin password protects
    /// the whole deployment, and the cost is paid once per enrolment rather than once per request.
    ///
    /// Measured: about 1.5 seconds per hash *and* per verification on a laptop. That is the intended
    /// trade — it is also why nothing on a request path may ever verify one, and why the command that
    /// does has to say it is working rather than appearing to hang.
    pub const DEPLOYMENT: Self = Self {
        memory_kib: 64 * 1024,
        passes: 3,
        lanes: 4,
    };

    /// The cheapest parameters the library accepts, for tests.
    ///
    /// Exposed because a test that spends eighty milliseconds per hash is a test suite that grows a
    /// minute of latency for no added confidence. Nothing in a deployment should use this.
    pub const TESTING: Self = Self {
        memory_kib: 8,
        passes: 1,
        lanes: 1,
    };
}

impl Default for Cost {
    fn default() -> Self {
        Self::DEPLOYMENT
    }
}

/// A password, stretched and encoded.
///
/// The string is a PHC hash: algorithm, parameters, salt and digest in one value, which is why
/// verification does not need to know what cost it was created with. It is still sensitive — it is
/// offline-crackable — so it lives wherever secrets live, not in a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Stretch a password.
    pub fn new(password: &str, cost: Cost) -> Result<Self> {
        check_length(password)?;
        let params = Params::new(cost.memory_kib, cost.passes, cost.lanes, None)
            .map_err(|error| Error::internal(format!("argon2 parameters are unusable: {error}")))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let salt = SaltString::generate(&mut rand_core::OsRng);
        let hash = argon
            .hash_password(password.as_bytes(), &salt)
            .map_err(|error| Error::internal(format!("cannot hash the password: {error}")))?;

        tracing::info!(
            event = "recovery_password_set",
            algorithm = "argon2id",
            memory_kib = cost.memory_kib,
            passes = cost.passes,
        );
        Ok(Self(hash.to_string()))
    }

    /// Read a hash that was stored earlier.
    pub fn from_stored(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        // Parsed here rather than at verification: a corrupted row should be reported where it is
        // read, not as a mysterious "wrong password" the first time somebody tries to recover.
        Phc::new(&value)
            .map_err(|error| Error::invalid(format!("the stored password hash is unusable: {error}")))?;
        Ok(Self(value))
    }

    /// The value to store.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `candidate` is the password.
    ///
    /// Returns `Ok(false)` for a wrong password and `Err` only when the *stored* value is broken —
    /// a caller must not be able to tell "wrong password" from "no such password" by anything but
    /// this boolean.
    pub fn verify(&self, candidate: &str) -> Result<bool> {
        let parsed = Phc::new(&self.0).map_err(|error| {
            Error::invalid(format!("the stored password hash is unusable: {error}"))
        })?;
        // The algorithm and cost come from the stored string, so a hash made with older parameters
        // keeps verifying after a deployment raises them.
        let verified = Argon2::default()
            .verify_password(candidate.as_bytes(), &parsed)
            .is_ok();
        tracing::debug!(event = "recovery_password_checked", matched = verified);
        Ok(verified)
    }
}

impl std::fmt::Display for PasswordHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Printed as a marker rather than the value: a hash that ends up in a log line is a hash
        // somebody can attack offline.
        f.write_str("<recovery password hash>")
    }
}

fn check_length(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(Error::invalid(format!(
            "a recovery password needs at least {MIN_PASSWORD_LEN} characters: it is the credential \\
             that re-enrols administrator keys when every one of them is lost"
        )));
    }
    Ok(())
}

/// A cap on guesses **within one process**.
///
/// See the module note: this stops a person at a keyboard, not an attacker with the database file.
/// It is in memory on purpose — persisting an attempt counter would need a writer for a value whose
/// whole purpose is to slow a human down for a few minutes.
#[derive(Debug)]
pub struct Limiter {
    window: Duration,
    allowed: u32,
    seen: std::sync::Mutex<Vec<Instant>>,
}

impl Limiter {
    /// `allowed` attempts per `window`.
    #[must_use]
    pub fn new(allowed: u32, window: Duration) -> Self {
        Self {
            window,
            allowed,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The deployment default: five attempts a minute.
    #[must_use]
    pub fn deployment() -> Self {
        Self::new(5, Duration::from_secs(60))
    }

    /// Whether an attempt may go ahead. Does not record it.
    pub fn ready(&self) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        seen.retain(|at| now.duration_since(*at) < self.window);
        seen.len() < self.allowed as usize
    }

    /// Record a failed attempt.
    pub fn record(&self) {
        let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        seen.push(Instant::now());
        tracing::warn!(
            event = "recovery_attempt_rejected",
            attempts = seen.len(),
            detail = "too many recovery attempts; waiting for the window to pass",
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn hash(password: &str) -> PasswordHash {
        PasswordHash::new(password, Cost::TESTING).unwrap()
    }

    #[test]
    fn the_right_password_verifies_and_a_wrong_one_does_not() {
        let stored = hash("correct horse battery");
        assert!(stored.verify("correct horse battery").unwrap());
        assert!(!stored.verify("correct horse batterY").unwrap());
        assert!(!stored.verify("").unwrap());
    }

    #[test]
    fn the_hash_does_not_contain_the_password() {
        // The obvious failure: a "hash" that is really the password with a prefix.
        let password = "correct horse battery";
        let stored = hash(password);
        assert!(!stored.as_str().contains(password));
        assert!(stored.as_str().starts_with("$argon2id$"), "{}", stored.as_str());
    }

    #[test]
    fn the_stored_string_carries_its_own_parameters() {
        // Raised costs must not invalidate existing hashes, which is why the PHC string is the value
        // rather than a struct with separate fields.
        let stored = PasswordHash::new(
            "correct horse battery",
            Cost {
                memory_kib: 8,
                passes: 2,
                lanes: 1,
            },
        )
        .unwrap();
        assert!(stored.as_str().contains("m=8,t=2,p=1"), "{}", stored.as_str());
        assert!(stored.verify("correct horse battery").unwrap());
    }

    #[test]
    fn a_short_password_is_refused_with_the_reason() {
        let error = PasswordHash::new("short", Cost::TESTING).unwrap_err();
        assert!(error.reason().contains("at least 12"), "{error:?}");
    }

    #[test]
    fn a_broken_stored_value_is_reported_when_read_not_when_guessed() {
        let error = PasswordHash::from_stored("not a phc string").unwrap_err();
        assert!(error.reason().contains("unusable"), "{error:?}");
    }

    #[test]
    fn the_hash_prints_as_a_marker() {
        // A hash in a log line is a hash somebody can attack offline.
        let stored = hash("correct horse battery");
        assert_eq!(format!("{stored}"), "<recovery password hash>");
    }

    #[test]
    fn the_limiter_holds_for_its_window() {
        let limiter = Limiter::new(2, Duration::from_millis(40));
        assert!(limiter.ready());
        limiter.record();
        assert!(limiter.ready());
        limiter.record();
        assert!(!limiter.ready(), "two attempts in the window is the cap");

        std::thread::sleep(Duration::from_millis(60));
        assert!(limiter.ready(), "the window passes");
    }
}
