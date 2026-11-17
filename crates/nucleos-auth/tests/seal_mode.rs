//! A seal key that others can read is not a key.
//!
//! It is the one file whose confidentiality is the whole property: the data directory holds ciphertext,
//! and a copy of this file beside a copy of that directory is the deployment.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos_auth::seal::SealKey;

#[test]
fn a_group_readable_key_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let path = std::env::temp_dir().join(format!("nucleos-seal-mode-{}", std::process::id()));
    let (_, bytes) = SealKey::generate();
    nucleos_auth::seal::write_key(&path, &bytes).unwrap();

    // What `write_key` writes: private, and accepted.
    assert!(SealKey::resolve(&path).unwrap().is_some());

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = SealKey::resolve(&path).expect_err("a world-readable key is not a key");
    assert!(
        error.reason().contains("0600"),
        "the refusal says what to do: {}",
        error.reason()
    );

    let _ = std::fs::remove_file(&path);
}
