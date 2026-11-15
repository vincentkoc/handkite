use super::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

pub(crate) fn fixture() -> (tempfile::TempDir, StateLayout, PendingProvisioning) {
    let (scratch, layout) = crate::layout::tests::fixture();
    let pending = PendingProvisioning::reserve_at(&layout).unwrap();
    (scratch, layout, pending)
}

pub(crate) fn load(layout: &StateLayout) -> Result<MacosProvisionedState, &'static str> {
    macos::load(layout)
}

pub(crate) fn is_reserved(layout: &StateLayout) -> bool {
    PendingProvisioning::reserve_at(layout).is_err()
}

fn state(identifier: [u8; 16]) -> MacosProvisionedState {
    MacosProvisionedState::parse(
        serde_json::to_vec(&serde_json::json!({
            "provider": "macos-adid", "version": 1, "binary_sha256": "ab".repeat(32),
            "identifier": identifier, "opaque": {"synthetic": "preserved"},
        }))
        .unwrap(),
        identifier,
    )
    .unwrap()
}

#[test]
fn reservation_is_private_immutable_random_and_survives_drop() {
    let (_scratch, layout, pending) = fixture();
    let path = layout.anisette_state_directory();
    let attempt = fs::read(path.join("attempt.json")).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&attempt).unwrap();
    assert_eq!(record["origin"], "https://anisette.example.invalid");
    assert_eq!(record["backend"], "macos-adid");
    assert_eq!(record["version"], 1);
    assert_eq!(
        record["identifier"],
        serde_json::json!(pending.identifier())
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(
        fs::metadata(path.join("attempt.json")).unwrap().mode() & 0o7777,
        0o400
    );
    let identifier = *pending.identifier();
    let (_other_scratch, _other_layout, other) = fixture();
    assert_ne!(identifier, *other.identifier());
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
    drop(pending);
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
    assert_eq!(fs::read(path.join("attempt.json")).unwrap(), attempt);
}

#[test]
fn concurrent_reservation_has_exactly_one_winner() {
    let (_scratch, layout) = crate::layout::tests::fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let layout = layout.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                PendingProvisioning::reserve_at(&layout).is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
}

#[test]
fn every_existing_entry_blocks_reservation_without_repair() {
    for kind in ["empty", "partial", "stage", "file", "symlink"] {
        let (_scratch, layout) = crate::layout::tests::fixture();
        let path = layout.anisette_state_directory();
        match kind {
            "file" => fs::write(&path, b"existing").unwrap(),
            "symlink" => symlink(layout.state_directory(), &path).unwrap(),
            _ => {
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                if kind != "empty" {
                    fs::write(
                        path.join(if kind == "stage" {
                            "ready.stage"
                        } else {
                            "attempt.json"
                        }),
                        b"partial",
                    )
                    .unwrap();
                }
            }
        }
        let before = fs::symlink_metadata(&path).unwrap();
        assert!(is_reserved(&layout));
        assert!(load(&layout).is_err());
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode(), before.len()),
            (after.dev(), after.ino(), after.mode(), after.len())
        );
    }
}

#[test]
fn ready_round_trip_preserves_opaque_bytes_and_attempt_inode() {
    let (_scratch, layout, pending) = fixture();
    let path = layout.anisette_state_directory();
    let attempt = fs::metadata(path.join("attempt.json")).unwrap();
    let value = state(*pending.identifier());
    pending.persist(&value).unwrap();
    assert_eq!(load(&layout).unwrap().as_bytes(), value.as_bytes());
    assert!(is_reserved(&layout));
    assert!(!path.join("ready.stage").exists());
    assert_eq!(
        fs::metadata(path.join("ready.json")).unwrap().mode() & 0o7777,
        0o400
    );
    let after = fs::metadata(path.join("attempt.json")).unwrap();
    assert_eq!(
        (attempt.ino(), attempt.ctime(), attempt.ctime_nsec()),
        (after.ino(), after.ctime(), after.ctime_nsec())
    );
}

#[test]
fn identity_mismatch_and_existing_stage_or_ready_never_overwrite() {
    for name in ["mismatch", "ready.stage", "ready.json"] {
        let (_scratch, layout, pending) = fixture();
        let path = layout.anisette_state_directory().join(name);
        let mut identifier = *pending.identifier();
        if name == "mismatch" {
            identifier[0] ^= 1;
        } else {
            fs::write(&path, b"existing").unwrap();
        }
        assert!(pending.persist(&state(identifier)).is_err());
        assert!(is_reserved(&layout));
        assert!(load(&layout).is_err());
        if name != "mismatch" {
            assert_eq!(fs::read(path).unwrap(), b"existing");
        }
    }
}

#[test]
fn reservation_rejects_unprotected_or_symlinked_ancestors() {
    for path_kind in ["root", "state"] {
        let (_scratch, layout) = crate::layout::tests::fixture();
        let path = if path_kind == "root" {
            layout.root()
        } else {
            layout.state_directory()
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PendingProvisioning::reserve_at(&layout).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let moved = path.with_extension("held");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(PendingProvisioning::reserve_at(&layout).is_err());
        assert!(!layout.anisette_state_directory().exists());
    }
}

#[test]
fn changed_ancestor_or_reservation_fails_without_publishing() {
    for ancestor in [true, false] {
        let (_scratch, layout, pending) = fixture();
        let value = state(*pending.identifier());
        let path = if ancestor {
            layout.state_directory()
        } else {
            layout.anisette_state_directory()
        };
        let moved = path.with_extension("held");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(pending.persist(&value).is_err());
        assert!(
            !layout
                .anisette_state_directory()
                .join("ready.json")
                .exists()
        );
        assert!(!moved.join("ready.json").exists());
    }
}

#[test]
fn ready_load_rejects_file_substitution_access_changes_and_residue() {
    for alteration in [
        "mode",
        "symlink",
        "hardlink",
        "directory",
        "oversize",
        "residue",
        "malformed",
    ] {
        let (_scratch, layout, pending) = fixture();
        let value = state(*pending.identifier());
        pending.persist(&value).unwrap();
        let root = layout.anisette_state_directory();
        let ready = root.join("ready.json");
        match alteration {
            "mode" => fs::set_permissions(&ready, fs::Permissions::from_mode(0o600)).unwrap(),
            "symlink" => {
                let held = root.join("held");
                fs::rename(&ready, &held).unwrap();
                symlink(&held, &ready).unwrap();
            }
            "hardlink" => fs::hard_link(&ready, layout.state_directory().join("linked")).unwrap(),
            "directory" => {
                fs::remove_file(&ready).unwrap();
                fs::create_dir(&ready).unwrap();
            }
            "residue" => fs::write(root.join("ready.stage"), b"partial").unwrap(),
            _ => {
                fs::set_permissions(&ready, fs::Permissions::from_mode(0o600)).unwrap();
                fs::write(
                    &ready,
                    if alteration == "oversize" {
                        vec![b'x'; super::super::MAX_ADI_STATE_BYTES + 1]
                    } else {
                        b"malformed".to_vec()
                    },
                )
                .unwrap();
                fs::set_permissions(&ready, fs::Permissions::from_mode(0o400)).unwrap();
            }
        }
        assert!(load(&layout).is_err(), "{alteration}");
        assert!(is_reserved(&layout));
    }
}

#[test]
fn ready_load_rejects_attempt_identity_origin_backend_version_and_schema() {
    for field in ["identifier", "origin", "backend", "version", "extra"] {
        let (_scratch, layout, pending) = fixture();
        let value = state(*pending.identifier());
        pending.persist(&value).unwrap();
        let path = layout.anisette_state_directory().join("attempt.json");
        let mut attempt: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        attempt[field] = match field {
            "identifier" => serde_json::json!([1; 16].to_vec()),
            "version" => serde_json::json!(2),
            _ => serde_json::json!("wrong"),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, serde_json::to_vec(&attempt).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(load(&layout).is_err(), "{field}");
        assert!(is_reserved(&layout));
    }
}

#[test]
fn acl_grants_on_state_files_and_directories_fail_closed() {
    for target in ["parent", "attempt", "ready"] {
        let (_scratch, layout, pending) = fixture();
        let value = state(*pending.identifier());
        pending.persist(&value).unwrap();
        let path = match target {
            "parent" => layout.state_directory(),
            "attempt" => layout.anisette_state_directory().join("attempt.json"),
            _ => layout.anisette_state_directory().join("ready.json"),
        };
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(load(&layout).is_err());
        assert!(is_reserved(&layout));
    }
}
