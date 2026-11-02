//! `nucleos init`: the one command that has to run before anything else can be configured.
//!
//! Everything after this arrives as a signed request over the ABI. That is the point of doing it first:
//! a deployment whose configuration is a file has two sources of truth from the moment the daemon
//! starts writing to its store, and this command is where the file would have been.
//!
//! What it creates, and why each thing has to exist before the daemon can be useful:
//!
//! | Thing | Why now |
//! | --- | --- |
//! | two stores | there is nowhere to record a later change without them |
//! | recovery password | the answer to "every administrator key is lost" |
//! | administrator key | the identity every later change is signed by; private half to the keychain |
//! | bootstrap policy | the first rule set, because the default is deny and an empty one locks everyone out |
//! | client socket file | how a client finds the daemon without being told |

use nucleos::error::{Error, Result};
use nucleos_auth::{Cost, KeyPair, KeyStore, MIN_PASSWORD_LEN, PasswordHash, RECOVERY_KEY};
use nucleos_sqlite::SystemStore;
use std::path::{Path, PathBuf};

/// Where an administrator's public key is filed in the system store, one row per key.
pub const ADMIN_KEYS_KEY: &str = "auth/admin_keys";

/// Where the bootstrap policy lives.
pub const POLICY_KEY: &str = "config/policy";

/// The policy a fresh deployment gets.
///
/// Deny everything, and allow the one administrator `init` created to configure the deployment.
///
/// Narrow in the dimension that matters — *who* — and complete in the dimension that has exactly one
/// user at this point. An earlier version allowed only `models` and `sources`, which read as prudent and
/// made the deployment unusable: the first administrator could not issue a tenant token, and could not
/// change the policy that stopped them. A bootstrap that has to be escaped is a bootstrap that teaches
/// people to escape it.
pub const BOOTSTRAP_POLICY: &str = r#"{
  "default": "deny",
  "rules": [
    {
      "decision": "allow",
      "config": "*",
      "scope": "system",
      "tenant": "default",
      "subject": "admin",
      "reason": "the bootstrap administrator configures this deployment; nobody else may do anything"
    }
  ]
}"#;

/// What `init` was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// The data directory.
    pub root: PathBuf,
    /// Where the socket will be, written into the client's discovery file.
    pub socket: PathBuf,
    /// The recovery password.
    pub password: String,
    /// Where the private key goes. Absent means the keychain.
    pub key_out: Option<PathBuf>,
    /// The tenant this first administrator belongs to.
    pub tenant: String,
    /// Their name within it.
    pub subject: String,
    /// Where the sealing key goes. **Never inside the root**, by default at `/etc/nucleos/seal.key`.
    pub seal_key: PathBuf,
    /// Where the client's discovery file goes. Absent means nowhere, which is right for a container.
    pub client_config: Option<PathBuf>,
    /// Cheaper hashing, for tests. Never for a real deployment.
    pub cost: Cost,
    /// Overwrite an existing installation.
    pub force: bool,
}

impl Options {
    /// The defaults a first run would use.
    #[must_use]
    pub fn new(root: PathBuf, socket: PathBuf, password: String) -> Self {
        Self {
            seal_key: default_seal_key(),
            root,
            socket,
            password,
            key_out: None,
            tenant: "default".to_owned(),
            subject: "admin".to_owned(),
            client_config: default_client_config(),
            cost: Cost::DEPLOYMENT,
            force: false,
        }
    }
}

/// What `init` did, for the summary it prints and for a test to check.
#[derive(Debug, Clone)]
pub struct Done {
    /// The key's short name, as it will appear in logs and refusals.
    pub key_id: String,
    /// Where the private half went, described without the secret.
    pub key_store: String,
}

/// Create a deployment.
pub async fn run(options: &Options) -> Result<Done> {
    if options.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(Error::invalid(format!(
            "a recovery password needs at least {MIN_PASSWORD_LEN} characters"
        )));
    }

    std::fs::create_dir_all(&options.root).map_err(|error| {
        Error::invalid(format!("cannot create {}: {error}", options.root.display()))
    })?;
    if let Some(parent) = options.socket.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::invalid(format!("cannot create {}: {error}", parent.display()))
        })?;
    }

    let system = SystemStore::open(options.root.join("system.db"))?;

    // Refusing rather than rewriting: a second `init` on a live deployment would replace the recovery
    // password and add another administrator key, and the operator who ran it would have been trying
    // to do something else.
    if !options.force
        && let Some(existing) = system.get(RECOVERY_KEY)?
    {
        let _ = existing;
        return Err(Error::invalid(format!(
            "{} is already initialised; use `nucleos admin recover` to re-enrol a key, or pass \
                 --force to replace this installation",
            options.root.display()
        )));
    }

    // 1. The recovery password.
    let hash = PasswordHash::new(&options.password, options.cost)?;
    system.set(RECOVERY_KEY, hash.as_str()).await?;

    // 2. The administrator's key. The private half goes to the keychain; the daemon is told the public
    //    half and nothing else, which is what makes a copy of its database useless for administration.
    let key = KeyPair::generate();
    let public = key.public(&options.tenant, &options.subject);
    let store = match &options.key_out {
        Some(path) => KeyStore::File(path.clone()),
        None => KeyStore::keychain(public.id.clone()),
    };
    store.put(&key.secret())?;
    system
        .set(
            ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![&public]).map_err(|error| {
                Error::internal(format!("cannot encode the public key: {error}"))
            })?,
        )
        .await?;

    // 3. The key that seals credentials in the store.
    //
    // Refused inside the root: a key stored beside the ciphertext it opens makes the sealing decorative,
    // and the root is exactly the directory people back up. A deployment that cannot write to
    // `/etc/nucleos` passes `--seal-key`, and a test passes a temp path.
    if options.seal_key.starts_with(&options.root) {
        return Err(Error::invalid(format!(
            "the seal key must live outside the data directory: {} is inside {}",
            options.seal_key.display(),
            options.root.display()
        )));
    }
    let (_, seal_bytes) = nucleos_auth::SealKey::generate();
    nucleos_auth::seal::write_key(&options.seal_key, &seal_bytes)?;

    // 4. The policy it starts from.
    system.set(POLICY_KEY, BOOTSTRAP_POLICY).await?;

    // 5. How a client finds the daemon, and which key signs for it.
    if let Some(client_config) = &options.client_config {
        write_client_config(client_config, &options.socket, &public.id)?;
    }

    tracing::info!(
        event = "initialised",
        root = %options.root.display(),
        key_id = %public.id,
        key_store = %store.describe(),
        socket = %options.socket.display(),
    );

    Ok(Done {
        key_id: public.id,
        key_store: store.describe(),
    })
}

/// Remember where the daemon listens and which key signs, for a client that was not told.
///
/// `key=value` lines rather than JSON: two answers to two questions, both of which a person may want
/// to read or edit by hand. A format with nesting for this would be a schema nobody needs.
fn write_client_config(path: &Path, socket: &Path, key_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::invalid(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    std::fs::write(path, format!("socket={}\nkey={key_id}\n", socket.display()))
        .map_err(|error| Error::invalid(format!("cannot write {}: {error}", path.display())))
}

/// The config tree, matching `nucleosd`'s default: not the data directory, which is what gets copied.
fn default_seal_key() -> PathBuf {
    PathBuf::from("/etc/nucleos/seal.key")
}

/// `~/.config/nucleos/socket` on Linux, `~/Library/Application Support/nucleos/socket` on macOS.
#[must_use]
pub fn default_client_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/nucleos/socket")
    } else {
        home.join(".config/nucleos/socket")
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn options(root: &Path) -> Options {
        let mut options = Options::new(
            root.to_path_buf(),
            root.join("run/nucleosd.sock"),
            "correct horse battery".to_owned(),
        );
        options.cost = Cost::TESTING;
        options.key_out = Some(root.join("admin.key"));
        // Outside the root, which is the rule: a key beside the ciphertext it opens seals nothing.
        options.seal_key = root.with_extension("seal.key");
        options.client_config = Some(root.join("client/socket"));
        options
    }

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nucleos-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(path.with_extension("seal.key"));
        path
    }

    #[tokio::test]
    async fn init_writes_everything_a_daemon_needs_to_start() {
        let root = scratch("init");
        let options = options(&root);
        let done = run(&options).await.unwrap();
        assert_eq!(done.key_id.len(), 8);

        let system = SystemStore::open(root.join("system.db")).unwrap();
        // The password is verifiable and is not stored in the clear.
        let hash = PasswordHash::from_stored(system.get(RECOVERY_KEY).unwrap().unwrap()).unwrap();
        assert!(hash.verify("correct horse battery").unwrap());
        // The bootstrap policy parses and permits exactly what it says.
        let policy = system.get(POLICY_KEY).unwrap().unwrap();
        let document = nucleos_policy::Document::parse(&policy).unwrap();
        document.validate().unwrap();
        assert_eq!(
            document.rules.len(),
            1,
            "one administrator, and every subject available to them"
        );
        // The administrator's public key is there, and the private one is at the path asked for.
        let keys = system.get(ADMIN_KEYS_KEY).unwrap().unwrap();
        assert!(keys.contains(&done.key_id));
        assert!(root.join("admin.key").exists());
        // A client can find the daemon.
        let client = std::fs::read_to_string(root.join("client/socket")).unwrap();
        assert!(
            client.contains(&format!("socket={}", options.socket.display())),
            "{client}"
        );
        assert!(client.contains(&format!("key={}", done.key_id)), "{client}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_second_init_refuses_rather_than_replacing_a_live_installation() {
        let root = scratch("init-twice");
        let options = options(&root);
        run(&options).await.unwrap();

        let error = run(&options)
            .await
            .expect_err("a live deployment is not re-initialised");
        assert!(error.reason().contains("already initialised"), "{error:?}");
        assert!(
            error.reason().contains("--force"),
            "and it says how: {error:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn force_replaces_it_deliberately() {
        let root = scratch("init-force");
        let mut options = options(&root);
        run(&options).await.unwrap();

        options.force = true;
        options.password = "another horse battery".to_owned();
        run(&options).await.unwrap();

        let system = SystemStore::open(root.join("system.db")).unwrap();
        let hash = PasswordHash::from_stored(system.get(RECOVERY_KEY).unwrap().unwrap()).unwrap();
        assert!(hash.verify("another horse battery").unwrap());
        assert!(!hash.verify("correct horse battery").unwrap());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_short_password_is_refused_before_anything_is_written() {
        let root = scratch("init-short");
        let mut options = options(&root);
        options.password = "short".to_owned();
        let error = run(&options).await.expect_err("refused");
        assert!(error.reason().contains("at least 12"), "{error:?}");
        assert!(!root.join("system.db").exists(), "nothing was created");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_bootstrap_policy_is_one_a_deployment_can_start_with() {
        // Deny by default, and enough permission to add a model — otherwise the first thing an
        // operator does is the thing that fails.
        let document = nucleos_policy::Document::parse(BOOTSTRAP_POLICY).unwrap();
        assert_eq!(document.unmatched, nucleos_policy::Unmatched::Deny);
        assert_eq!(document.rules.len(), 1, "one administrator, every subject");
        assert_eq!(
            document.rules[0].decision,
            Some(nucleos_policy::Decision::Allow)
        );
        assert_eq!(document.rules[0].config.as_deref(), Some("*"));
        assert_eq!(document.rules[0].subject.as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn a_seal_key_inside_the_data_directory_is_refused() {
        // The default location is the one place it must not be, so a deployment that points it there
        // gets an error rather than a seal that protects nothing.
        let root = scratch("init-seal-inside");
        let mut options = options(&root);
        options.seal_key = root.join("seal.key");
        let error = run(&options)
            .await
            .expect_err("a key beside the ciphertext seals nothing");
        assert!(
            error.reason().contains("outside the data directory"),
            "{error:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
