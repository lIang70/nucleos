//! `nucleos admin recover`: enrol a key when every administrator key is lost.
//!
//! The one command that uses the recovery password, and the reason the password exists at all. It is
//! **local by construction**: it reads the store from the filesystem and never talks to the daemon,
//! because a remote recovery path would be an oracle for guessing the one credential that can recreate
//! administration.
//!
//! What it does, in order: verify the password against the stored hash, generate a new keypair, enrol
//! the *public* half beside the existing keys, and put the private half where an administrator keeps it
//! — the keychain, or a named file. It does not remove the old keys: an operator who has lost a machine
//! is not necessarily an operator who wants every other administrator revoked.

use nucleos::error::{Error, Result};
use nucleos_auth::{Cost, KeyPair, KeyStore, Limiter, PasswordHash, RECOVERY_KEY};
use nucleos_sqlite::SystemStore;
use std::path::PathBuf;

/// What the operator said.
#[derive(Debug, Clone)]
pub struct Options {
    /// The data directory.
    pub root: PathBuf,
    /// The recovery password.
    pub password: String,
    /// Where the new private key goes. Absent means the keychain.
    pub key_out: Option<PathBuf>,
    /// Who the new key acts as.
    pub tenant: String,
    /// Their name within it.
    pub subject: String,
    /// Cheaper hashing for a test that has to run in milliseconds.
    pub cost: Cost,
}

impl Options {
    /// The defaults a recovery would use.
    #[must_use]
    pub fn new(root: PathBuf, password: String) -> Self {
        Self {
            root,
            password,
            key_out: None,
            tenant: "default".to_owned(),
            subject: "admin".to_owned(),
            cost: Cost::DEPLOYMENT,
        }
    }
}

/// What recovery did.
#[derive(Debug, Clone)]
pub struct Done {
    /// The new key's short name.
    pub key_id: String,
    /// Where the private half went, described without the secret.
    pub key_store: String,
    /// How many keys the deployment now trusts.
    pub enrolled: usize,
}

/// Enrol a new administrator key, given the recovery password.
pub async fn recover(options: &Options) -> Result<Done> {
    let system = SystemStore::open(options.root.join("system.db"))?;

    let stored = system.get(RECOVERY_KEY)?.ok_or_else(|| {
        Error::invalid(format!(
            "{} is not initialised: run `nucleos init` first",
            options.root.display()
        ))
    })?;
    let hash = PasswordHash::from_stored(stored)?;

    // Five attempts a minute: this stops a person at a keyboard, not an attacker with the store file —
    // that one is answered by the hash cost, which is why the cost is this high.
    let limiter = Limiter::deployment();
    if !limiter.ready() {
        return Err(Error::denied(
            "too many recovery attempts: wait a minute before trying again",
        ));
    }
    if !hash.verify(&options.password)? {
        limiter.record();
        return Err(Error::denied("the recovery password is not valid"));
    }

    // A fresh keypair: recovery restores administration, it does not restore the old key. A private key
    // that was lost is a private key that may be in somebody else's hands.
    let key = KeyPair::generate();
    let public = key.public(&options.tenant, &options.subject);
    let store = match &options.key_out {
        Some(path) => KeyStore::File(path.clone()),
        None => KeyStore::keychain(public.id.clone()),
    };
    store.put(&key.secret())?;

    let mut keys = enrolled(&system)?;
    keys.push(public.clone());
    let text = serde_json::to_string(&keys)
        .map_err(|error| Error::internal(format!("cannot encode the keys: {error}")))?;
    system.set(ADMIN_KEYS_KEY, &text).await?;
    system
        .record(nucleos_sqlite::Change {
            at_unix_millis: now(),
            tenant: options.tenant.clone(),
            subject: options.subject.clone(),
            key_id: public.id.clone(),
            key: "admin_keys".to_owned(),
            value: format!("recover {}", public.id),
        })
        .await?;

    tracing::info!(
        event = "admin_key_recovered",
        id = %public.id,
        enrolled = keys.len(),
    );

    Ok(Done {
        key_id: public.id,
        key_store: store.describe(),
        enrolled: keys.len(),
    })
}

/// Where administrator keys live. The same key the daemon reads.
const ADMIN_KEYS_KEY: &str = "auth/admin_keys";

fn enrolled(system: &SystemStore) -> Result<Vec<nucleos_auth::PublicKey>> {
    match system.get(ADMIN_KEYS_KEY)? {
        Some(raw) => serde_json::from_str(&raw).map_err(|error| {
            Error::internal(format!("stored administrator keys are unreadable: {error}"))
        }),
        None => Ok(Vec::new()),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
