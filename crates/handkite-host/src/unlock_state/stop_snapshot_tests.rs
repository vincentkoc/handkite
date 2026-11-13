use super::*;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::PathBuf;

struct Fixture {
    _scratch: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(scratch.path()).unwrap().join("account");
        fs::create_dir(&home).unwrap();
        let mut state = home.clone();
        for name in super::super::STATE_COMPONENTS {
            state.push(name);
            fs::create_dir(&state).unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let fixture = Self {
            _scratch: scratch,
            home,
            state,
        };
        fixture.write("UNLOCK-DISABLED");
        fixture.write("UNLOCK-DISABLED.prior-trial.hold");
        fixture
    }

    fn write(&self, name: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.state.join(name))
            .unwrap();
        file.write_all(b"uncertain\n").unwrap();
    }

    fn capture(&self) -> Vec<u8> {
        macos::capture_at(&self.home).unwrap()
    }
}

#[test]
fn receipt_pins_all_stops_without_contents_or_account_paths() {
    let fixture = Fixture::new();
    let before = fixture.capture();
    assert_eq!(compare(&before, &fixture.capture()), Ok(()));
    let text = std::str::from_utf8(&before).unwrap();
    assert!(!text.contains(fixture.home.to_str().unwrap()));
    assert!(!text.contains("uncertain"));
    let parsed: Snapshot = serde_json::from_slice(&before).unwrap();
    assert_eq!(parsed.markers.len(), 2);
    for marker in parsed.markers {
        assert_eq!(
            marker.identity.inode,
            fs::metadata(fixture.state.join(marker.name)).unwrap().ino()
        );
    }
}

#[test]
fn success_restore_keeps_receipt_but_failure_rotation_requires_new_review() {
    let fixture = Fixture::new();
    let before = fixture.capture();
    let active = fixture.state.join("UNLOCK-DISABLED");
    let hold = fixture.state.join("UNLOCK-DISABLED.next-trial.hold");
    fs::rename(&active, &hold).unwrap();
    assert!(macos::capture_at(&fixture.home).is_err());
    fs::rename(&hold, &active).unwrap();
    assert_eq!(compare(&before, &fixture.capture()), Ok(()));
    fs::rename(&active, &hold).unwrap();
    fixture.write("UNLOCK-DISABLED");
    assert_eq!(compare(&before, &fixture.capture()), Err(CHANGED));
    assert_eq!(fs::read(hold).unwrap(), b"uncertain\n");
}

#[test]
fn missing_changed_added_or_replaced_hold_refuses() {
    for change in ["remove", "change", "add", "replace"] {
        let fixture = Fixture::new();
        let before = fixture.capture();
        let hold = fixture.state.join("UNLOCK-DISABLED.prior-trial.hold");
        match change {
            "remove" => fs::remove_file(&hold).unwrap(),
            "change" => fs::write(&hold, b"changed\n").unwrap(),
            "add" => fixture.write("UNLOCK-DISABLED.unreviewed.hold"),
            "replace" => {
                let replacement = fixture.state.join("replacement");
                fs::rename(&hold, &replacement).unwrap();
                fixture.write("UNLOCK-DISABLED.prior-trial.hold");
            }
            _ => unreachable!(),
        }
        assert_eq!(
            compare(&before, &fixture.capture()),
            Err(CHANGED),
            "{change}"
        );
    }
}

#[test]
fn unsafe_entries_and_ancestors_refuse_without_mutation() {
    for kind in [
        "symlink",
        "hardlink",
        "mode",
        "directory",
        "name",
        "large",
        "parent",
    ] {
        let fixture = Fixture::new();
        let active = fixture.state.join("UNLOCK-DISABLED");
        match kind {
            "symlink" => {
                fs::remove_file(&active).unwrap();
                symlink("UNLOCK-DISABLED.prior-trial.hold", &active).unwrap();
            }
            "hardlink" => {
                fs::hard_link(&active, fixture.state.join("alias")).unwrap();
            }
            "mode" => fs::set_permissions(&active, fs::Permissions::from_mode(0o644)).unwrap(),
            "directory" => {
                fs::remove_file(&active).unwrap();
                fs::create_dir(&active).unwrap();
            }
            "name" => fixture.write("UNLOCK-DISABLED.unrecognized"),
            "large" => fs::write(&active, vec![0; 4097]).unwrap(),
            "parent" => {
                let original = fixture.state.with_extension("original");
                fs::rename(&fixture.state, &original).unwrap();
                symlink(&original, &fixture.state).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(macos::capture_at(&fixture.home).is_err(), "{kind}");
    }
}

#[test]
fn receipt_parser_rejects_duplicate_extra_or_noncanonical_input() {
    let fixture = Fixture::new();
    let actual = fixture.capture();
    for input in [
        b"{}".as_slice(),
        b"{\"version\":1,\"version\":1}",
        b"[]",
        b"null",
    ] {
        assert_eq!(compare(input, &actual), Err(INVALID));
    }
    let mut parsed: serde_json::Value = serde_json::from_slice(&actual).unwrap();
    parsed["extra"] = true.into();
    assert_eq!(
        compare(&serde_json::to_vec(&parsed).unwrap(), &actual),
        Err(INVALID)
    );
}

#[test]
fn check_refuses_nonprivate_or_linked_receipts_before_account_access() {
    let fixture = Fixture::new();
    for mode in [0o644, 0o400] {
        let path = fixture.home.join(format!("receipt-{mode}"));
        fs::write(&path, fixture.capture()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(check(&mut File::open(path).unwrap()), Err(INVALID));
    }
    let path = fixture.home.join("receipt-linked");
    fs::write(&path, fixture.capture()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, fixture.home.join("alias")).unwrap();
    assert_eq!(check(&mut File::open(path).unwrap()), Err(INVALID));
}

#[test]
fn private_receipt_is_validated_before_state_capture_and_never_refreshed() {
    let fixture = Fixture::new();
    let path = fixture.home.join("receipt");
    let original = fixture.capture();
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        check_with(&mut File::open(&path).unwrap(), || Ok(fixture.capture())),
        Ok(())
    );
    fixture.write("UNLOCK-DISABLED.new.hold");
    assert_eq!(
        check_with(&mut File::open(&path).unwrap(), || Ok(fixture.capture())),
        Err(CHANGED)
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::write(&path, b"{}\n").unwrap();
    assert_eq!(
        check_with(&mut File::open(&path).unwrap(), || panic!(
            "invalid receipt must not read state"
        )),
        Err(INVALID)
    );
}

#[test]
fn concurrent_file_and_directory_changes_refuse() {
    for change in ["content", "replace", "parent"] {
        let fixture = Fixture::new();
        let active = fixture.state.join("UNLOCK-DISABLED");
        let result = macos::capture_checked(&fixture.home, || match change {
            "content" => fs::write(&active, b"changed\n").unwrap(),
            "replace" => {
                fs::rename(&active, fixture.state.join("UNLOCK-DISABLED.new.hold")).unwrap();
                fixture.write("UNLOCK-DISABLED");
            }
            "parent" => {
                fs::rename(&fixture.state, fixture.state.with_extension("retained")).unwrap();
                fs::create_dir(&fixture.state).unwrap();
            }
            _ => unreachable!(),
        });
        assert!(result.is_err(), "{change}");
    }
}
