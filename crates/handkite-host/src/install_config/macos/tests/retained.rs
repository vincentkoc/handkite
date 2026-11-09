use super::*;

fn protected(layout: &ServiceLayout) -> Vec<(std::path::PathBuf, Fingerprint)> {
    [
        layout.config_file(),
        layout.config_file().parent().unwrap().into(),
        layout.process_lock(),
        layout.state_directory().join("UNLOCK-DISABLED"),
        layout.state_directory().join("state.json"),
        layout.current_release_selection(),
    ]
    .into_iter()
    .map(|path| {
        let before = fs::symlink_metadata(&path).unwrap().into();
        (path, before)
    })
    .collect()
}

fn private_state(layout: &ServiceLayout) {
    for name in ["UNLOCK-DISABLED", "state.json"] {
        let path = layout.state_directory().join(name);
        fs::write(&path, b"preserved-private-state").unwrap();
        chmod(&path, 0o600);
    }
    symlink("ab".repeat(32), layout.current_release_selection()).unwrap();
}

fn unchanged(before: Vec<(std::path::PathBuf, Fingerprint)>) {
    for (path, original) in before {
        assert_eq!(original, fs::symlink_metadata(path).unwrap().into());
    }
}

#[test]
fn retains_original_bytes_parsed_binding_readonly_descriptors_and_lock() {
    let (_scratch, layout) = fixture();
    private_state(&layout);
    let owner = InstallerLock::acquire(&layout).unwrap();
    let before = protected(&layout);
    let mut independent = expected();
    let mut value = RetainedConfig::acquire(&owner, &independent).unwrap();
    independent.key = DeviceKey::new("changed-claim").unwrap();
    independent.usb_id = HardwareId::new("changed-identity").unwrap();
    assert!(std::ptr::eq(value._owner, &owner));
    assert_eq!(value.binding().key, expected().key);
    assert_eq!(value.binding().usb_id, expected().usb_id);
    assert_eq!(&*value.config.bytes, CONFIG);
    assert_eq!(
        value.config.held.fingerprint,
        fs::metadata(layout.config_file()).unwrap().into()
    );
    let flags = unsafe { libc::fcntl(value.config.held.file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    for _ in 0..3 {
        value.revalidate().unwrap();
    }
    drop(value);
    unchanged(before);
    assert_eq!(fs::read(layout.config_file()).unwrap(), CONFIG);
}

#[test]
fn acquisition_matches_both_independent_fields_without_side_effects() {
    let (_scratch, layout) = fixture();
    private_state(&layout);
    let owner = InstallerLock::acquire(&layout).unwrap();
    let before = protected(&layout);
    for independent in [
        DeviceBinding {
            key: DeviceKey::new("other").unwrap(),
            ..expected()
        },
        DeviceBinding {
            usb_id: HardwareId::new("other-device").unwrap(),
            ..expected()
        },
    ] {
        assert_eq!(
            RetainedConfig::acquire(&owner, &independent).err(),
            Some(UNSAFE)
        );
    }
    unchanged(before);
}

#[test]
fn unsafe_config_is_neither_retained_nor_repaired() {
    for mutation in 0..9 {
        let (_scratch, layout) = fixture();
        let owner = InstallerLock::acquire(&layout).unwrap();
        let path = layout.config_file();
        match mutation {
            0 => fs::remove_file(&path).unwrap(),
            1 => {
                fs::rename(&path, path.with_extension("original")).unwrap();
                symlink("host.original", &path).unwrap();
            }
            2 => fs::hard_link(&path, path.with_extension("link")).unwrap(),
            3 => chmod(&path, 0o644),
            4 => acl(&path, "everyone allow read"),
            5 => chmod(path.parent().unwrap(), 0o750),
            6 => acl(path.parent().unwrap(), "everyone deny delete"),
            7 => fs::write(&path, b"invalid config").unwrap(),
            _ => fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap(),
        }
        let file_before = fs::symlink_metadata(&path).ok().map(Fingerprint::from);
        let parent_before = Fingerprint::from(fs::metadata(path.parent().unwrap()).unwrap());
        assert_eq!(
            RetainedConfig::acquire(&owner, &expected()).err(),
            Some(UNSAFE)
        );
        assert_eq!(
            file_before,
            fs::symlink_metadata(&path).ok().map(Fingerprint::from)
        );
        assert_eq!(
            parent_before,
            fs::metadata(path.parent().unwrap()).unwrap().into()
        );
        if mutation == 6 {
            clear_acl(path.parent().unwrap());
        }
    }
}

#[test]
fn same_byte_file_or_parent_replacement_never_resets_the_original_baseline() {
    for replace_parent in [false, true] {
        let (_scratch, layout) = fixture();
        let owner = InstallerLock::acquire(&layout).unwrap();
        let mut original = RetainedConfig::acquire(&owner, &expected()).unwrap();
        let path = layout.config_file();
        if replace_parent {
            let parent = path.parent().unwrap();
            fs::rename(parent, parent.with_extension("original")).unwrap();
            fs::create_dir(parent).unwrap();
            chmod(parent, 0o700);
        } else {
            fs::rename(&path, path.with_extension("original")).unwrap();
        }
        fs::write(&path, CONFIG).unwrap();
        chmod(&path, 0o600);
        let mut fresh = RetainedConfig::acquire(&owner, &expected()).unwrap();
        fresh.revalidate().unwrap();
        for _ in 0..2 {
            assert_eq!(original.revalidate(), Err(UNSAFE));
        }
        assert_eq!(original.binding().key, expected().key);
        assert_eq!(original.binding().usb_id, expected().usb_id);
    }
}

#[test]
fn exact_original_bytes_include_accepted_whitespace_and_bounded_repeated_reads() {
    let (_scratch, layout) = fixture();
    let mut bytes = CONFIG.to_vec();
    bytes.resize(MAX_CONFIG_BYTES as usize, b' ');
    fs::write(layout.config_file(), &bytes).unwrap();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut value = RetainedConfig::acquire(&owner, &expected()).unwrap();
    assert_eq!(&*value.config.bytes, bytes);
    for _ in 0..2 {
        value.revalidate().unwrap();
    }
    // Same parsed binding is insufficient after changing the retained encoding.
    *bytes.last_mut().unwrap() = b'\n';
    fs::write(layout.config_file(), &bytes).unwrap();
    super::super::super::verify(&layout, &expected()).unwrap();
    assert_eq!(value.revalidate(), Err(UNSAFE));
    bytes.push(b' ');
    fs::write(layout.config_file(), bytes).unwrap();
    assert_eq!(value.revalidate(), Err(UNSAFE));
}

#[test]
fn changes_after_read_fail_acquisition_and_retained_revalidation() {
    for acquiring in [false, true] {
        for mutation in 0..7 {
            let (_scratch, layout) = fixture();
            private_state(&layout);
            let owner = InstallerLock::acquire(&layout).unwrap();
            let mut value = RetainedConfig::acquire(&owner, &expected()).unwrap();
            let preserved: Vec<_> = protected(&layout).into_iter().skip(3).collect();
            let path = layout.config_file();
            let mut change = || match mutation {
                0 => fs::write(&path, [CONFIG, b" "].concat()).unwrap(),
                1 => {
                    fs::rename(&path, path.with_extension("original")).unwrap();
                    fs::write(&path, CONFIG).unwrap();
                    chmod(&path, 0o600);
                }
                2 => chmod(&path, 0o644),
                3 => acl(&path, "everyone allow read"),
                4 => fs::hard_link(&path, path.with_extension("link")).unwrap(),
                5 => chmod(&layout.process_lock(), 0o644),
                _ => {
                    let parent = path.parent().unwrap();
                    fs::rename(parent, parent.with_extension("original")).unwrap();
                    fs::create_dir(parent).unwrap();
                    chmod(parent, 0o700);
                }
            };
            let result = if acquiring {
                retain(&owner, &expected(), Some(&mut change)).map(|_| ())
            } else {
                revalidate(&mut value, Some(&mut change))
            };
            assert_eq!(result, Err(UNSAFE));
            unchanged(preserved);
        }
    }
}

#[test]
fn invalid_original_lock_refuses_retention_and_revalidation() {
    let (_scratch, layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut value = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let config_before = Fingerprint::from(fs::metadata(layout.config_file()).unwrap());
    chmod(&layout.process_lock(), 0o644);
    assert_eq!(value.revalidate(), Err(UNSAFE));
    assert_eq!(
        RetainedConfig::acquire(&owner, &expected()).err(),
        Some(UNSAFE)
    );
    chmod(&layout.process_lock(), 0o600);
    assert_eq!(value.revalidate(), Err(UNSAFE));
    assert_eq!(
        config_before,
        fs::metadata(layout.config_file()).unwrap().into()
    );
}
