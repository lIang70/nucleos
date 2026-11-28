#![allow(clippy::unwrap_used)]

use super::files::subset_or_all;
use super::*;
use std::collections::BTreeMap;

fn files(read: &[&str], write: &[&str], read_only: bool) -> Files {
    Files {
        read: read.iter().map(|p| (*p).to_owned()).collect(),
        write: write.iter().map(|p| (*p).to_owned()).collect(),
        read_only,
    }
}

#[test]
fn the_root_folds_the_list_into_everything() {
    let normalized = files(&["/", "/srv"], &[], true).normalized().unwrap();
    assert!(normalized.read.is_empty(), "empty means all");
}

#[test]
fn a_subtree_absorbs_what_it_covers() {
    let normalized = files(&["/srv/a/b", "/srv/a"], &[], true)
        .normalized()
        .unwrap();
    assert_eq!(normalized.read, vec!["/srv/a".to_owned()]);
}

#[test]
fn a_relative_root_is_refused() {
    assert!(files(&["srv"], &[], true).normalized().is_err());
}

#[test]
fn read_only_and_write_are_mutually_exclusive() {
    let err = files(&["/srv"], &["/srv"], true).normalized().unwrap_err();
    assert!(err.message.contains("mutually exclusive"), "{err:?}");
}

#[test]
fn a_narrowed_read_with_unrestricted_write_is_refused() {
    let err = files(&["/srv"], &[], false).normalized().unwrap_err();
    assert!(err.message.contains("write is unrestricted"), "{err:?}");
}

#[test]
fn a_write_root_must_be_readable() {
    let err = files(&["/srv/a"], &["/other"], false)
        .normalized()
        .unwrap_err();
    assert!(err.message.contains("not readable"), "{err:?}");
}

#[test]
fn dropping_a_write_root_narrows() {
    let wide = files(&["/srv"], &["/srv/a", "/srv/b"], false)
        .normalized()
        .unwrap();
    let narrow = files(&["/srv"], &["/srv/a"], false).normalized().unwrap();
    assert!(wide.narrows_to(&narrow));
    assert!(!narrow.narrows_to(&wide));
}

#[test]
fn read_only_narrows_but_cannot_be_left() {
    let write = files(&["/srv"], &["/srv/a"], false).normalized().unwrap();
    let ro = files(&["/srv"], &[], true).normalized().unwrap();
    assert!(
        write.narrows_to(&ro),
        "dropping the write root is a narrowing"
    );
    assert!(!ro.narrows_to(&write), "gaining writes is a widening");

    // Turning writes off is not a licence to read more.
    let wide_read = files(&[], &[], true).normalized().unwrap();
    assert!(
        !files(&["/srv"], &[], true)
            .normalized()
            .unwrap()
            .narrows_to(&wide_read),
        "a read-only policy that reads everything is wider, not narrower"
    );
}

#[test]
fn an_absent_list_is_unrestricted_and_so_is_not_a_subset() {
    assert!(!subset_or_all(&["/srv".to_owned()], &[]));
    assert!(subset_or_all(&[], &["/anything".to_owned()]));
}

#[test]
fn a_stricter_network_posture_narrows_and_a_looser_one_widens() {
    let default = NetPolicy::default();
    let deny = NetPolicy {
        mode: NetMode::DenyAll,
        ..NetPolicy::default()
    };
    assert!(default.narrows_to(&deny));
    assert!(!deny.narrows_to(&default));
}

#[test]
fn turning_tls_interception_off_is_a_widening() {
    let on = NetPolicy {
        mitm: Some(Mitm {
            enabled: true,
            ..Mitm::default()
        }),
        ..NetPolicy::default()
    };
    let off = NetPolicy::default();
    assert!(off.narrows_to(&on));
    assert!(!on.narrows_to(&off));
}

#[test]
fn an_environment_list_may_shrink_but_not_grow() {
    let wide = EnvPolicy {
        allow: vec!["PATH".to_owned(), "HOME".to_owned()],
        inject: BTreeMap::new(),
    };
    let narrow = EnvPolicy {
        allow: vec!["PATH".to_owned()],
        inject: BTreeMap::new(),
    };
    assert!(wide.narrows_to(&narrow));
    assert!(!narrow.narrows_to(&wide));
}

#[test]
fn a_call_may_ask_for_less_and_gets_the_ceiling_when_it_asks_for_more() {
    let ceiling = Resources {
        cpu_millicores: 500,
        memory_bytes: Some(1 << 30),
        disk_bytes: None,
        max_output_bytes: Some(1 << 20),
    };
    let asked = Resources {
        cpu_millicores: 2000,
        memory_bytes: Some(1 << 31),
        disk_bytes: Some(1 << 40),
        max_output_bytes: None,
    };
    let effective = ceiling.narrowed_to(&asked);
    assert_eq!(
        effective.cpu_millicores, 500,
        "asking for more gets the ceiling"
    );
    assert_eq!(effective.memory_bytes, Some(1 << 30));
    assert_eq!(
        effective.disk_bytes,
        Some(1 << 40),
        "the ceiling was unbounded"
    );
    assert_eq!(effective.max_output_bytes, Some(1 << 20));
}

#[test]
fn a_backend_without_a_file_boundary_refuses_a_write_restriction() {
    let enforcement = Enforcement::default();
    let confine = Confine {
        files: files(&["/srv"], &["/srv/a"], false).normalized().unwrap(),
        ..Confine::default()
    };
    let error = enforcement.can_enforce(&confine).unwrap_err();
    assert_eq!(error.kind, crate::ErrorKind::Unsupported);
}

#[test]
fn a_network_posture_the_backend_cannot_enforce_is_refused_at_construction() {
    let enforcement = Enforcement {
        net_modes: vec![NetMode::AllowList],
        ..Enforcement::default()
    };
    let confine = Confine {
        net: NetPolicy {
            mode: NetMode::DenyAll,
            ..NetPolicy::default()
        },
        ..Confine::default()
    };
    assert!(enforcement.can_enforce(&confine).is_err());
}

#[test]
fn default_network_is_never_an_enforceable_posture() {
    let enforcement = Enforcement::default();
    let confine = Confine::default();
    assert!(
        enforcement.can_enforce(&confine).is_ok(),
        "the absence of a posture needs no enforcement"
    );
}
