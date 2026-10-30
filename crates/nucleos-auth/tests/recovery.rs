//! The recovery password, as a deployment stores it: stretched, written to the system store, and
//! verified there.
//!
//! The composition is the point. Either half alone is easy: a hash function that nobody stores, or a
//! key-value table that holds a plaintext password. These tests put them together and ask the two
//! questions that matter — can the password be checked later, and does reading the row give an
//! attacker anything usable?

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos_auth::{Cost, Limiter, PasswordHash, RECOVERY_KEY};
use nucleos_sqlite::SystemStore;
use std::time::Duration;

const PASSWORD: &str = "correct horse battery staple";

#[tokio::test]
async fn a_password_written_at_init_is_verified_after_a_restart() {
    let path = std::env::temp_dir().join(format!("nucleos-cred-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);

    {
        let store = SystemStore::open(&path).unwrap();
        let hash = PasswordHash::new(PASSWORD, Cost::TESTING).unwrap();
        store.set(RECOVERY_KEY, hash.as_str()).await.unwrap();
    }

    // A second process, the same file: this is the "every key is lost, recover" path.
    let store = SystemStore::open(&path).unwrap();
    let stored = store.get(RECOVERY_KEY).unwrap().expect("the hash was written");
    let hash = PasswordHash::from_stored(stored).unwrap();

    assert!(hash.verify(PASSWORD).unwrap());
    assert!(!hash.verify("correct horse battery stapl").unwrap());

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn the_stored_row_is_not_a_password() {
    let store = SystemStore::in_memory().unwrap();
    let hash = PasswordHash::new(PASSWORD, Cost::TESTING).unwrap();
    store.set(RECOVERY_KEY, hash.as_str()).await.unwrap();

    let row = store.get(RECOVERY_KEY).unwrap().unwrap();
    // What a leaked store gives: an Argon2 hash, and the cost parameters needed to attack it
    // offline. That is a slow problem, not an immediate one — which is the whole point of the cost.
    assert!(!row.contains(PASSWORD));
    assert!(row.starts_with("$argon2id$"));
}

#[tokio::test]
async fn an_uninitialised_deployment_says_so_rather_than_failing_a_login() {
    let store = SystemStore::in_memory().unwrap();
    // No row is a state, not an error: `init` has not run, and recovery has nothing to check against.
    assert_eq!(store.get(RECOVERY_KEY).unwrap(), None);
}

#[test]
fn recovery_is_rate_limited_within_the_process() {
    // Five attempts a minute is the deployment default; the limiter here uses a shorter window so the
    // test does not sleep for a minute to watch it expire.
    let limiter = Limiter::new(2, Duration::from_millis(30));
    let hash = PasswordHash::new(PASSWORD, Cost::TESTING).unwrap();

    for _ in 0..2 {
        assert!(limiter.ready());
        assert!(!hash.verify("guess").unwrap());
        limiter.record();
    }
    assert!(!limiter.ready(), "the third guess inside the window is refused");

    std::thread::sleep(Duration::from_millis(50));
    assert!(limiter.ready(), "and the window passing restores it");
}
