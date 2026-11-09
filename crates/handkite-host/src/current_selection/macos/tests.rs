use super::*;
use crate::current_selection::encode_target;
use crate::service_layout::tests::fixture_in;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

const MANIFEST: ManifestDigest = [0x13; 32];

fn fixture(present: bool) -> (tempfile::TempDir, ServiceLayout) {
    let fixture = fixture_in(&std::env::current_dir().unwrap());
    if present {
        select(&fixture.1, &encode_target(&MANIFEST));
    }
    fixture
}

fn select(layout: &ServiceLayout, target: &[u8]) {
    symlink(
        OsStr::from_bytes(target),
        layout.current_release_selection(),
    )
    .unwrap();
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn acl(path: &Path, rule: &str) {
    assert!(
        Command::new("/bin/chmod")
            .args(["+a", rule])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

fn clear_acl(path: &Path) {
    assert!(
        Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn observes_absence_and_dangling_selection_without_changing_private_state() {
    let (_scratch, layout) = fixture(false);
    let marker = layout.state_directory().join("UNLOCK-DISABLED");
    fs::write(&marker, b"existing-stop\n").unwrap();
    chmod(&marker, 0o600);
    let marker_before = Fingerprint::from(fs::metadata(&marker).unwrap());
    assert_eq!(super::super::inspect(&layout), Ok(None));
    assert_eq!(
        fs::read_dir(layout.releases_directory()).unwrap().count(),
        0
    );
    select(&layout, &encode_target(&MANIFEST));
    let link_before =
        Fingerprint::from(fs::symlink_metadata(layout.current_release_selection()).unwrap());
    for _ in 0..2 {
        assert_eq!(super::super::inspect(&layout), Ok(Some(MANIFEST)));
    }
    assert!(!layout.current_release_selection().exists());
    assert_eq!(
        link_before,
        Fingerprint::from(fs::symlink_metadata(layout.current_release_selection()).unwrap())
    );
    assert_eq!(
        marker_before,
        Fingerprint::from(fs::metadata(&marker).unwrap())
    );
    assert_eq!(fs::read(marker).unwrap(), b"existing-stop\n");
    assert_eq!(
        fs::read_dir(layout.releases_directory()).unwrap().count(),
        1
    );
}

#[test]
fn never_follows_or_validates_a_destination() {
    let (_scratch, layout) = fixture(true);
    let destination = layout
        .releases_directory()
        .join(OsStr::from_bytes(&encode_target(&MANIFEST)));
    fs::write(&destination, b"not a release").unwrap();
    chmod(&destination, 0);
    assert_eq!(super::super::inspect(&layout), Ok(Some(MANIFEST)));
    chmod(&destination, 0o600);
    fs::remove_file(&destination).unwrap();
    symlink("/no-such-selection-fixture", &destination).unwrap();
    assert_eq!(super::super::inspect(&layout), Ok(Some(MANIFEST)));
}

#[test]
fn rejects_nonlinks_and_hardlinked_symlinks_without_blocking() {
    let (_scratch, layout) = fixture(false);
    let path = layout.current_release_selection();
    fs::write(&path, encode_target(&MANIFEST)).unwrap();
    assert!(super::super::inspect(&layout).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(super::super::inspect(&layout).is_err());
    fs::remove_dir(&path).unwrap();
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(super::super::inspect(&layout).is_err());
    fs::remove_file(&path).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("s");
    let _listener = UnixListener::bind(&socket).unwrap();
    fs::rename(socket, &path).unwrap();
    assert!(super::super::inspect(&layout).is_err());
    fs::remove_file(&path).unwrap();
    select(&layout, &encode_target(&MANIFEST));
    fs::hard_link(&path, path.with_extension("alias")).unwrap();
    assert_eq!(fs::symlink_metadata(&path).unwrap().nlink(), 2);
    assert!(super::super::inspect(&layout).is_err());
}

#[test]
fn rejects_noncanonical_raw_targets() {
    let (_scratch, layout) = fixture(false);
    let valid = encode_target(&MANIFEST);
    for target in [
        valid[..63].to_vec(),
        [valid.as_slice(), b"0"].concat(),
        [b"./", &valid[..62]].concat(),
        [b"/", &valid[..63]].concat(),
        [b"0x", &valid[..62]].concat(),
        vec![b'A'; 64],
        vec![b'.'; 64],
        vec![b' '; 64],
        vec![b'\n'; 64],
        vec![0xff; 64],
    ] {
        select(&layout, &target);
        assert_eq!(super::super::inspect(&layout), Err(UNSAFE));
        fs::remove_file(layout.current_release_selection()).unwrap();
    }
}

#[test]
fn only_enoent_is_absence_and_metadata_policy_requires_owned_single_link() {
    let uid = unsafe { libc::geteuid() };
    assert_eq!(link_metadata(-1, uid), Err(UNSAFE));
    let (_scratch, layout) = fixture(false);
    fs::write(layout.config_file(), b"fixture").unwrap();
    let file = File::open(layout.config_file()).unwrap();
    assert_eq!(link_metadata(file.as_raw_fd(), uid), Err(UNSAFE));
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    metadata.st_mode = libc::S_IFLNK | 0o777;
    metadata.st_uid = uid;
    metadata.st_nlink = 1;
    metadata.st_size = 64;
    assert!(link_policy(&metadata, uid).is_ok());
    assert!(link_policy(&metadata, uid.wrapping_add(1)).is_err());
    for links in [0, 2] {
        metadata.st_nlink = links;
        assert!(link_policy(&metadata, uid).is_err());
    }
    metadata.st_nlink = 1;
    for size in [-1, 0, 63, 65] {
        metadata.st_size = size;
        assert!(link_policy(&metadata, uid).is_err());
    }
}

#[test]
fn rejects_missing_symlinked_unsafe_or_foreign_account_parents() {
    let (_scratch, layout) = fixture(true);
    for parent in layout.releases_directory().ancestors().take(5) {
        let moved = parent.with_extension("held");
        fs::rename(parent, &moved).unwrap();
        assert!(super::super::inspect(&layout).is_err());
        symlink(&moved, parent).unwrap();
        assert!(super::super::inspect(&layout).is_err());
        fs::remove_file(parent).unwrap();
        fs::rename(&moved, parent).unwrap();
        for mode in [0o770, 0o707, 0o1700] {
            chmod(parent, mode);
            assert!(super::super::inspect(&layout).is_err());
        }
        chmod(parent, 0o700);
    }
    chmod(&layout.releases_directory(), 0o755);
    assert!(super::super::inspect(&layout).is_err());
    chmod(&layout.releases_directory(), 0o700);
    let uid = unsafe { libc::geteuid() }.wrapping_add(1);
    let root = File::open("/").unwrap();
    assert!(directory_policy(&root, uid, false, true).is_err());
    let releases = File::open(layout.releases_directory()).unwrap();
    assert!(directory_policy(&releases, uid, true, true).is_err());
    assert_eq!(super::super::inspect(&layout), Ok(Some(MANIFEST)));
}

#[test]
fn rejects_acl_grants_and_private_acl_entries() {
    let (_scratch, layout) = fixture(true);
    for rule in ["everyone allow read", "everyone deny delete"] {
        acl(&layout.releases_directory(), rule);
        assert!(super::super::inspect(&layout).is_err());
        clear_acl(&layout.releases_directory());
    }
    acl(&layout.root(), "everyone allow read");
    assert!(super::super::inspect(&layout).is_err());
    clear_acl(&layout.root());
    acl(&layout.root(), "everyone deny delete");
    assert_eq!(super::super::inspect(&layout), Ok(Some(MANIFEST)));
    clear_acl(&layout.root());
}

#[test]
fn detects_changes_during_read_and_between_fresh_observations() {
    for point in [
        InspectionPoint::AfterMetadata,
        InspectionPoint::AfterReadlink,
        InspectionPoint::BetweenObservations,
    ] {
        for mutation in 0..9 {
            let (_scratch, layout) = fixture(true);
            let path = layout.current_release_selection();
            let mut changed = false;
            let mut hook = |at| {
                if at != point || changed {
                    return;
                }
                changed = true;
                match mutation {
                    0..=2 => {
                        fs::rename(&path, path.with_extension("held")).unwrap();
                        let target = match mutation {
                            0 => encode_target(&MANIFEST).to_vec(),
                            1 => encode_target(&[0x24; 32]).to_vec(),
                            _ => vec![b'a'; 65],
                        };
                        select(&layout, &target);
                    }
                    3 => fs::remove_file(&path).unwrap(),
                    4 => {
                        fs::remove_file(&path).unwrap();
                        fs::write(&path, encode_target(&MANIFEST)).unwrap();
                    }
                    5 | 6 => {
                        let directory = if mutation == 5 {
                            layout.releases_directory()
                        } else {
                            layout.root()
                        };
                        fs::rename(&directory, directory.with_extension("held")).unwrap();
                        fs::create_dir(&directory).unwrap();
                        chmod(&directory, 0o700);
                        if mutation == 5 {
                            select(&layout, &encode_target(&MANIFEST));
                        }
                    }
                    7 => chmod(&layout.releases_directory(), 0o755),
                    _ => acl(&layout.releases_directory(), "everyone allow read"),
                }
            };
            assert_eq!(inspect(&layout, Some(&mut hook)), Err(UNSAFE));
            assert!(changed);
        }
    }
}

#[test]
fn detects_absence_changing_and_private_parent_inventory_changes() {
    for present in [false, true] {
        let (_scratch, layout) = fixture(present);
        let mut hook = |point| {
            if point == InspectionPoint::BetweenObservations {
                if present {
                    fs::write(layout.releases_directory().join("unrelated"), b"changed").unwrap();
                } else {
                    select(&layout, &encode_target(&MANIFEST));
                }
            }
        };
        assert_eq!(inspect(&layout, Some(&mut hook)), Err(UNSAFE));
    }
}

#[test]
fn final_parent_check_rejects_change_after_the_second_link_read() {
    let (_scratch, layout) = fixture(true);
    let mut reads = 0;
    let mut hook = |point| {
        if point == InspectionPoint::AfterReadlink {
            reads += 1;
            if reads == 2 {
                fs::write(layout.releases_directory().join("unrelated"), b"changed").unwrap();
            }
        }
    };
    assert_eq!(inspect(&layout, Some(&mut hook)), Err(UNSAFE));
    assert_eq!(reads, 2);
}

#[test]
fn snapshot_preserves_absence_or_dangling_target_across_parent_entry_changes() {
    for present in [false, true] {
        let (_scratch, layout) = fixture(present);
        let snapshot = Snapshot::capture(&layout).unwrap();
        assert_eq!(
            snapshot.observation.as_ref().map(|value| value.manifest),
            present.then_some(MANIFEST)
        );
        assert!(!layout.current_release_selection().exists());
        snapshot.reobserve_unchanged().unwrap();
        let sibling = layout.releases_directory().join("unrelated");
        fs::create_dir(&sibling).unwrap();
        chmod(&sibling, 0o700);
        snapshot.reobserve_unchanged().unwrap();
        fs::remove_dir(&sibling).unwrap();
        snapshot.reobserve_unchanged().unwrap();
        if present {
            let metadata = fs::symlink_metadata(layout.current_release_selection()).unwrap();
            let observed = snapshot.observation.as_ref().unwrap();
            assert_eq!(observed.fingerprint.inode, metadata.ino());
            assert_eq!(observed.fingerprint.size, 64);
        }
    }
}

#[test]
fn snapshot_rejects_same_target_inode_replacement_and_presence_changes() {
    for present in [false, true] {
        for replacement in [false, true] {
            let (_scratch, layout) = fixture(present);
            let snapshot = Snapshot::capture(&layout).unwrap();
            if present {
                fs::rename(
                    layout.current_release_selection(),
                    layout.releases_directory().join("old-current"),
                )
                .unwrap();
            }
            if !present || replacement {
                select(&layout, &encode_target(&MANIFEST));
            }
            assert_eq!(snapshot.reobserve_unchanged(), Err(UNSAFE));
        }
    }
}

#[test]
fn snapshot_keeps_parent_identity_and_access_policy_fixed() {
    for change in 0..4 {
        let (_scratch, layout) = fixture(true);
        let snapshot = Snapshot::capture(&layout).unwrap();
        match change {
            0 => {
                let releases = layout.releases_directory();
                fs::rename(&releases, releases.with_extension("old")).unwrap();
                fs::create_dir(&releases).unwrap();
                chmod(&releases, 0o700);
                select(&layout, &encode_target(&MANIFEST));
            }
            1 => chmod(&layout.releases_directory(), 0o750),
            2 => acl(&layout.releases_directory(), "everyone allow read"),
            _ => {
                let root = layout.root();
                fs::rename(&root, root.with_extension("old")).unwrap();
                fs::create_dir(&root).unwrap();
                chmod(&root, 0o700);
            }
        }
        assert_eq!(snapshot.reobserve_unchanged(), Err(UNSAFE));
    }
}

#[test]
fn snapshot_reobserve_requires_a_stable_parent_during_its_own_read() {
    for change_link in [false, true] {
        let (_scratch, layout) = fixture(true);
        let snapshot = Snapshot::capture(&layout).unwrap();
        let mut hook = |point| {
            if point == InspectionPoint::BetweenObservations {
                if change_link {
                    fs::rename(
                        layout.current_release_selection(),
                        layout.releases_directory().join("old-current"),
                    )
                    .unwrap();
                    select(&layout, &encode_target(&MANIFEST));
                } else {
                    fs::write(layout.releases_directory().join("unrelated"), b"changed").unwrap();
                }
            }
        };
        assert_eq!(snapshot.reobserve(Some(&mut hook)), Err(UNSAFE));
    }
}
