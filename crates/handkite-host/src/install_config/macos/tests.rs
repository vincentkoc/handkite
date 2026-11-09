use super::*;
use crate::service_layout::tests::fixture_in;
use handkite_core::protocol::{DeviceKey, HardwareId};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

const CONFIG: &[u8] = br#"{"device_key":"phone","usb_id":"test-device"}"#;

mod retained;

fn expected() -> DeviceBinding {
    DeviceBinding {
        key: DeviceKey::new("phone").unwrap(),
        usb_id: HardwareId::new("test-device").unwrap(),
    }
}

fn fixture() -> (tempfile::TempDir, ServiceLayout) {
    let fixture = fixture_in(&std::env::current_dir().unwrap());
    fs::write(fixture.1.config_file(), CONFIG).unwrap();
    chmod(&fixture.1.config_file(), 0o600);
    fixture
}

fn check_both(layout: &ServiceLayout) -> Result<(), &'static str> {
    let loaded = super::super::load_protected(layout);
    let verified = super::super::verify(layout, &expected());
    assert_eq!(
        loaded.as_ref().map(|_| ()).map_err(|error| *error),
        verified
    );
    loaded.map(|binding| {
        assert_eq!(binding.key, expected().key);
        assert_eq!(binding.usb_id, expected().usb_id);
    })
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
fn reads_fixed_config_without_changing_config_or_protected_state() {
    let (_scratch, layout) = fixture();
    let marker = layout.state_directory().join("UNLOCK-DISABLED");
    fs::write(&marker, b"existing-stop\n").unwrap();
    chmod(&marker, 0o600);
    let before = [layout.config_file(), marker.clone()]
        .map(|path| Fingerprint::from(fs::metadata(path).unwrap()));
    let held = AnchoredConfig::open(&layout).unwrap();
    let flags = unsafe { libc::fcntl(held.file.as_raw_fd(), libc::F_GETFL) };
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    assert_ne!(flags & libc::O_NONBLOCK, 0);
    assert_ne!(
        unsafe { libc::fcntl(held.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    for _ in 0..2 {
        check_both(&layout).unwrap();
    }
    assert_eq!(fs::read(layout.config_file()).unwrap(), CONFIG);
    assert_eq!(fs::read(&marker).unwrap(), b"existing-stop\n");
    let after =
        [layout.config_file(), marker].map(|path| Fingerprint::from(fs::metadata(path).unwrap()));
    assert_eq!(before, after);
    assert_eq!(
        fs::read_dir(layout.config_file().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    for directory in [
        layout.runtime_directory(),
        layout.releases_directory(),
        layout
            .staged_launchd_descriptor()
            .parent()
            .unwrap()
            .to_path_buf(),
    ] {
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }
}

#[test]
fn pins_both_device_key_and_usb_identity() {
    let (_scratch, layout) = fixture();
    let wrong_key = DeviceBinding {
        key: DeviceKey::new("other").unwrap(),
        ..expected()
    };
    let wrong_usb = DeviceBinding {
        usb_id: HardwareId::new("other-device").unwrap(),
        ..expected()
    };
    for binding in [wrong_key, wrong_usb] {
        assert_eq!(super::super::verify(&layout, &binding), Err(UNSAFE));
    }
}

#[test]
fn loader_returns_the_configured_binding_without_an_independent_expectation() {
    let (_scratch, layout) = fixture();
    fs::write(
        layout.config_file(),
        br#"{"device_key":"other-phone","usb_id":"other-device"}"#,
    )
    .unwrap();
    let binding = super::super::load_protected(&layout).unwrap();
    assert_eq!(binding.key.as_str(), "other-phone");
    assert_eq!(binding.usb_id.as_str(), "other-device");
    assert_eq!(super::super::verify(&layout, &expected()), Err(UNSAFE));
}

#[test]
fn bounded_shared_parser_rejects_invalid_and_oversized_config() {
    let (_scratch, layout) = fixture();
    for content in [
        &b""[..],
        br#"{"device_key":"phone","usb_id":"test-device","extra":true}"#,
        br#"{"device_key":"phone","device_key":"phone","usb_id":"test-device"}"#,
        br#"{"device_key":"phone","usb_id":"test-device","usb_id":"test-device"}"#,
        br#"{"device_key":"../phone","usb_id":"test-device"}"#,
        br#"{"device_key":"phone","usb_id":"invalid identity"}"#,
        br#"{"device_key":"phone","usb_id":null}"#,
    ] {
        fs::write(layout.config_file(), content).unwrap();
        assert_eq!(check_both(&layout), Err(UNSAFE));
    }
    let mut padded = CONFIG.to_vec();
    padded.resize(MAX_CONFIG_BYTES as usize, b' ');
    fs::write(layout.config_file(), &padded).unwrap();
    check_both(&layout).unwrap();
    padded.push(b' ');
    fs::write(layout.config_file(), padded).unwrap();
    assert_eq!(check_both(&layout), Err(UNSAFE));
}

#[test]
fn rejects_missing_links_and_nonregular_files_without_blocking() {
    let (_scratch, layout) = fixture();
    let path = layout.config_file();
    let moved = path.with_extension("held");
    fs::rename(&path, &moved).unwrap();
    assert!(check_both(&layout).is_err());
    symlink(&moved, &path).unwrap();
    assert!(check_both(&layout).is_err());
    fs::remove_file(&path).unwrap();
    fs::hard_link(&moved, &path).unwrap();
    assert!(check_both(&layout).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(check_both(&layout).is_err());
    fs::remove_dir(&path).unwrap();
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(check_both(&layout).is_err());
    fs::remove_file(&path).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let socket = scratch.path().join("s");
    let _listener = UnixListener::bind(&socket).unwrap();
    fs::rename(socket, &path).unwrap();
    assert!(check_both(&layout).is_err());
}

#[test]
fn rejects_wrong_modes_and_foreign_owner_metadata() {
    let (_scratch, layout) = fixture();
    for mode in [0o400, 0o644, 0o660, 0o606, 0o700, 0o1600, 0o4600] {
        chmod(&layout.config_file(), mode);
        assert!(check_both(&layout).is_err());
    }
    chmod(&layout.config_file(), 0o600);
    let foreign = unsafe { libc::geteuid() }.wrapping_add(1);
    assert!(file_policy(&File::open(layout.config_file()).unwrap(), foreign).is_err());
    let config = File::open(layout.config_file().parent().unwrap()).unwrap();
    assert!(directory_policy(&config, foreign, true, true).is_err());
    let root = File::open("/").unwrap();
    assert!(directory_policy(&root, foreign, false, true).is_err());
    check_both(&layout).unwrap();
}

#[test]
fn anchors_each_account_parent_and_requires_private_config_directory() {
    let (_scratch, layout) = fixture();
    for parent in layout.config_file().parent().unwrap().ancestors().take(5) {
        let moved = parent.with_extension("held");
        fs::rename(parent, &moved).unwrap();
        assert!(check_both(&layout).is_err());
        symlink(&moved, parent).unwrap();
        assert!(check_both(&layout).is_err());
        fs::remove_file(parent).unwrap();
        fs::rename(&moved, parent).unwrap();
        for mode in [0o770, 0o707, 0o1700] {
            chmod(parent, mode);
            assert!(check_both(&layout).is_err());
        }
        chmod(parent, 0o700);
    }
    chmod(layout.config_file().parent().unwrap(), 0o755);
    assert!(check_both(&layout).is_err());
    chmod(layout.config_file().parent().unwrap(), 0o700);
    check_both(&layout).unwrap();
}

#[test]
fn rejects_acl_grants_and_private_acls_but_allows_ancestor_denials() {
    let (_scratch, layout) = fixture();
    for path in [
        layout.config_file(),
        layout.config_file().parent().unwrap().into(),
    ] {
        for rule in ["everyone allow read", "everyone deny delete"] {
            acl(&path, rule);
            assert!(check_both(&layout).is_err());
            clear_acl(&path);
        }
    }
    acl(&layout.root(), "everyone allow read");
    assert!(check_both(&layout).is_err());
    clear_acl(&layout.root());
    acl(&layout.root(), "everyone deny delete");
    check_both(&layout).unwrap();
    clear_acl(&layout.root());
}

#[test]
fn detects_changes_after_read_before_returning_a_binding_match() {
    for compare_expected in [false, true] {
        for mutation in 0..8 {
            let (_scratch, layout) = fixture();
            let path = layout.config_file();
            let mut change = || match mutation {
                0 => fs::write(
                    &path,
                    CONFIG
                        .iter()
                        .map(|byte| if *byte == b'p' { b'q' } else { *byte })
                        .collect::<Vec<_>>(),
                )
                .unwrap(),
                1 => {
                    fs::rename(&path, path.with_extension("held")).unwrap();
                    fs::write(&path, CONFIG).unwrap();
                    chmod(&path, 0o600);
                }
                2 => chmod(&path, 0o644),
                3 => fs::hard_link(&path, path.with_extension("link")).unwrap(),
                4 => acl(&path, "everyone allow read"),
                5 | 6 => {
                    let directory = if mutation == 5 {
                        path.parent().unwrap().to_path_buf()
                    } else {
                        layout.root()
                    };
                    fs::rename(&directory, directory.with_extension("held")).unwrap();
                    fs::create_dir(&directory).unwrap();
                    chmod(&directory, 0o700);
                }
                _ => fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap(),
            };
            let expected = expected();
            assert_eq!(
                read(
                    &layout,
                    compare_expected.then_some(&expected),
                    Some(&mut change)
                )
                .map(|_| ()),
                Err(UNSAFE)
            );
        }
    }
}
