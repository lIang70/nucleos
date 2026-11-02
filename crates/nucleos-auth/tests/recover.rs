//! Recovery: the path back in when every administrator key is lost.
//!
//! The composition is the point again — a password hash in the store, a keypair generated locally, and
//! a public key appended to the list the daemon checks. These tests drive the real store and the real
//! hashing, because the failure this path exists for is the one nobody can rehearse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos_auth::{Cost, PasswordHash, RECOVERY_KEY};
use nucleos_sqlite::SystemStore;

const PASSWORD: &str = "correct horse battery";
const ADMIN_KEYS_KEY: &str = "auth/admin_keys";

/// A store with a recovery password and no administrator keys — the state after losing every key.
async fn initialised(path: &std::path::Path) -> SystemStore {
    let _ = std::fs::remove_file(path);
    let store = SystemStore::open(path).unwrap();
    let hash = PasswordHash::new(PASSWORD, Cost::TESTING).unwrap();
    store.set(RECOVERY_KEY, hash.as_str()).await.unwrap();
    store
}

#[tokio::test]
async fn a_recovered_key_is_one_the_daemon_would_accept() {
    let path = std::env::temp_dir().join(format!("nucleos-recover-{}", std::process::id()));
    let store = initialised(&path).await;

    // Recovery as the command does it: verify the password, make a key, keep the public half.
    let stored = store.get(RECOVERY_KEY).unwrap().unwrap();
    let hash = PasswordHash::from_stored(stored).unwrap();
    assert!(hash.verify(PASSWORD).unwrap());

    let key = nucleos_auth::KeyPair::generate();
    let public = key.public("default", "admin");
    store
        .set(
            ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![public.clone()]).unwrap(),
        )
        .await
        .unwrap();

    // The property that matters: a signature from the recovered key verifies against what the store
    // now holds, which is exactly what the daemon's verifier reads on the next request.
    let stored_keys: Vec<nucleos_auth::PublicKey> =
        serde_json::from_str(&store.get(ADMIN_KEYS_KEY).unwrap().unwrap()).unwrap();
    let canonical = nucleos_auth::canonical("GET", "/v1/config", b"", 1_000, "nonce");
    let signature = key.sign(canonical.as_bytes());
    assert!(
        stored_keys[0]
            .verifies(canonical.as_bytes(), &signature)
            .unwrap()
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_wrong_password_verifies_nothing() {
    let path = std::env::temp_dir().join(format!("nucleos-recover-bad-{}", std::process::id()));
    let store = initialised(&path).await;

    let hash = PasswordHash::from_stored(store.get(RECOVERY_KEY).unwrap().unwrap()).unwrap();
    assert!(!hash.verify("correct horse batterY").unwrap());
    assert!(
        store.get(ADMIN_KEYS_KEY).unwrap().is_none(),
        "a failed check enrols nothing"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn recovery_does_not_revoke_the_keys_that_still_exist() {
    // Losing one machine is not a decision to revoke every other administrator.
    let path = std::env::temp_dir().join(format!("nucleos-recover-keep-{}", std::process::id()));
    let store = initialised(&path).await;

    let existing = nucleos_auth::KeyPair::generate().public("default", "other");
    store
        .set(
            ADMIN_KEYS_KEY,
            &serde_json::to_string(&vec![existing.clone()]).unwrap(),
        )
        .await
        .unwrap();

    let recovered = nucleos_auth::KeyPair::generate().public("default", "admin");
    let mut keys: Vec<nucleos_auth::PublicKey> =
        serde_json::from_str(&store.get(ADMIN_KEYS_KEY).unwrap().unwrap()).unwrap();
    keys.push(recovered.clone());
    store
        .set(ADMIN_KEYS_KEY, &serde_json::to_string(&keys).unwrap())
        .await
        .unwrap();

    let keys: Vec<nucleos_auth::PublicKey> =
        serde_json::from_str(&store.get(ADMIN_KEYS_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|key| key.id == existing.id));
    assert!(keys.iter().any(|key| key.id == recovered.id));

    let _ = std::fs::remove_file(&path);
}
