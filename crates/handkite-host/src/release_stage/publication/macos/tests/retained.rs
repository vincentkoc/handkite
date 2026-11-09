use super::*;

fn published(fixture: &Fixture) -> PublishedRelease<'_> {
    run(fixture.stage(), |_, _| Ok(())).unwrap()
}

fn reopen(fixture: &Fixture) -> Result<PublishedRelease<'_>, &'static str> {
    reopen_published(
        &fixture.owner,
        &fixture.bytes,
        &hash(&fixture.bytes),
        CERTIFICATE,
        Some(&mut |_, _| Ok(())),
    )
}

fn fingerprints(fixture: &Fixture) -> Vec<Fingerprint> {
    let mut paths = vec![
        fixture.layout().releases_directory(),
        fixture.directory(),
        fixture.receipt(),
        fixture.layout().process_lock(),
    ];
    paths.extend(NAMES.map(|name| fixture.directory().join(name.filename())));
    paths
        .into_iter()
        .map(|path| fs::symlink_metadata(path).unwrap().into())
        .collect()
}

#[test]
fn reopening_retains_exact_bytes_trust_readonly_inodes_and_lock_without_writes() {
    let fixture = Fixture::new();
    let original = published(&fixture);
    let before = fingerprints(&fixture);
    let protected = fixture.protected();
    let mut supplied = fixture.bytes.clone();
    let mut seen = Vec::new();
    let mut value = reopen_published(
        &fixture.owner,
        &supplied,
        &hash(&supplied),
        CERTIFICATE,
        Some(&mut |name, path| {
            seen.push((name, path.to_path_buf()));
            Ok(())
        }),
    )
    .unwrap();
    supplied.fill(b'!');
    assert_eq!(value.staged._canonical_manifest.as_ref(), fixture.bytes);
    assert_eq!(
        value.staged._trust.manifest().canonical_json().unwrap(),
        fixture.bytes
    );
    assert!(std::ptr::eq(value.staged._owner, &fixture.owner));
    assert_eq!(value.directory_name(), fixture.name());
    assert_eq!(value.receipt_name(), fixture.receipt_name());
    assert_eq!(
        value
            .staged
            ._destination
            .private_directory_identities()
            .unwrap(),
        original
            .staged
            ._destination
            .private_directory_identities()
            .unwrap()
    );
    for file in value
        .staged
        ._destination
        .artifacts
        .iter()
        .map(|file| &file.file)
        .chain([&value._receipt.file])
    {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
    }
    assert_eq!(
        seen,
        NAMES.map(|name| (name, fixture.directory().join(name.filename())))
    );
    revalidate(&mut value, Some(&mut |_, _| Ok(()))).unwrap();
    revalidate(&mut value, Some(&mut |_, _| Ok(()))).unwrap();
    assert_eq!(before, fingerprints(&fixture));
    fixture.unchanged(protected);
}

#[test]
fn public_opener_and_revalidation_require_the_real_signer() {
    let fixture = Fixture::new();
    let mut value = published(&fixture);
    let before = fingerprints(&fixture);
    let protected = fixture.protected();
    assert_eq!(
        crate::release_stage::reopen_published(
            &fixture.owner,
            &fixture.bytes,
            &hash(&fixture.bytes),
            CERTIFICATE,
        )
        .err(),
        Some("artifact signature does not satisfy signer policy")
    );
    assert_eq!(
        value.revalidate(),
        Err("artifact signature does not satisfy signer policy")
    );
    assert_eq!(before, fingerprints(&fixture));
    fixture.unchanged(protected);
}

#[test]
fn invalid_trust_is_rejected_before_observing_an_invalid_lock_or_paths() {
    let fixture = Fixture::new();
    chmod(&fixture.layout().process_lock(), 0o644);
    let mut noncanonical = fixture.bytes.clone();
    noncanonical.push(b'\n');
    for (bytes, pin, cert, expected) in [
        (
            &fixture.bytes,
            [0; 32],
            CERTIFICATE,
            "release manifest pin mismatch",
        ),
        (
            &noncanonical,
            hash(&noncanonical),
            CERTIFICATE,
            "release manifest is not canonical",
        ),
        (
            &fixture.bytes,
            hash(&fixture.bytes),
            "invalid",
            "invalid signer certificate SHA1",
        ),
    ] {
        assert_eq!(
            reopen_published(
                &fixture.owner,
                bytes,
                &pin,
                cert,
                Some(&mut |_, _| { panic!("invalid trust reached signer") })
            )
            .err(),
            Some(expected)
        );
    }
    assert_eq!(
        fs::read_dir(fixture.layout().releases_directory())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn missing_directory_receipt_or_both_never_repair_or_adopt_orphans() {
    for missing in 0..3 {
        let fixture = Fixture::new();
        drop(published(&fixture));
        if missing != 1 {
            fs::remove_dir_all(fixture.directory()).unwrap();
        }
        if missing != 0 {
            fs::remove_file(fixture.receipt()).unwrap();
        }
        let protected = fixture.protected();
        let before =
            Fingerprint::from(fs::metadata(fixture.layout().releases_directory()).unwrap());
        assert!(reopen(&fixture).is_err());
        assert_eq!(fixture.directory().exists(), missing == 1);
        assert_eq!(fixture.receipt().exists(), missing == 0);
        assert_eq!(
            before,
            fs::metadata(fixture.layout().releases_directory())
                .unwrap()
                .into()
        );
        fixture.unchanged(protected);
    }
}

#[test]
fn receipt_bytes_modes_links_acls_and_special_files_fail_before_signer() {
    for mutation in 0..9 {
        let fixture = Fixture::new();
        drop(published(&fixture));
        let path = fixture.receipt();
        match mutation {
            0 => chmod(&path, 0o600),
            1..=3 => {
                chmod(&path, 0o600);
                let bytes = match mutation {
                    1 => vec![b'!'; fixture.bytes.len()],
                    2 => fixture.bytes[..fixture.bytes.len() - 1].to_vec(),
                    _ => vec![b'!'; crate::release_manifest::MAX_MANIFEST_BYTES + 1],
                };
                fs::write(&path, bytes).unwrap();
                chmod(&path, 0o400);
            }
            4 => {
                fs::hard_link(&path, path.with_extension("link")).unwrap();
            }
            5 => acl(&path),
            _ => {
                fs::remove_file(&path).unwrap();
                match mutation {
                    6 => symlink("missing", &path).unwrap(),
                    7 => fs::create_dir(&path).unwrap(),
                    _ => {
                        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o400) }, 0);
                    }
                }
            }
        }
        let protected = fixture.protected();
        assert!(
            reopen_published(
                &fixture.owner,
                &fixture.bytes,
                &hash(&fixture.bytes),
                CERTIFICATE,
                Some(&mut |_, _| panic!("unsafe receipt reached signer")),
            )
            .is_err()
        );
        fixture.unchanged(protected);
    }
}

#[test]
fn unsafe_ancestors_private_directories_and_inventory_are_refused() {
    for mutation in 0..7 {
        let fixture = Fixture::new();
        drop(published(&fixture));
        match mutation {
            0 => chmod(&fixture.layout().releases_directory(), 0o750),
            1 => acl(&fixture.layout().releases_directory()),
            2 => chmod(&fixture.directory(), 0o750),
            3 => acl(&fixture.directory()),
            4 => {
                let moved = fixture.directory().with_extension("moved");
                fs::rename(fixture.directory(), &moved).unwrap();
                symlink(moved, fixture.directory()).unwrap();
            }
            5 => chmod(
                fixture.layout().releases_directory().parent().unwrap(),
                0o777,
            ),
            _ => fs::write(fixture.directory().join("unexpected"), b"extra").unwrap(),
        }
        let protected = fixture.protected();
        assert!(reopen(&fixture).is_err());
        fixture.unchanged(protected);
    }
}

fn replace_file(path: &Path) {
    let bytes = fs::read(path).unwrap();
    let mode = fs::metadata(path).unwrap().mode() & 0o7777;
    fs::rename(path, path.with_extension("original")).unwrap();
    fs::write(path, bytes).unwrap();
    chmod(path, mode);
    fs::remove_file(path.with_extension("original")).unwrap();
}

#[test]
fn revalidation_never_accepts_same_byte_replacement_or_resets_fingerprints() {
    for mutation in 0..4 {
        let fixture = Fixture::new();
        let mut value = published(&fixture);
        let protected = fixture.protected();
        match mutation {
            0 => replace_file(&fixture.receipt()),
            1 => replace_file(&fixture.directory().join(NAMES[0].filename())),
            2 => {
                let moved = fixture.directory().with_extension("original");
                fs::rename(fixture.directory(), &moved).unwrap();
                fs::create_dir(fixture.directory()).unwrap();
                chmod(&fixture.directory(), 0o700);
                for name in NAMES {
                    let target = fixture.directory().join(name.filename());
                    fs::copy(moved.join(name.filename()), &target).unwrap();
                    chmod(&target, 0o500);
                }
            }
            _ => {
                let path = fixture.layout().releases_directory();
                fs::rename(&path, path.with_extension("original")).unwrap();
                fs::create_dir(&path).unwrap();
                chmod(&path, 0o700);
            }
        }
        for _ in 0..2 {
            assert!(
                revalidate(
                    &mut value,
                    Some(&mut |_, _| { panic!("changed pair reached signer") })
                )
                .is_err()
            );
        }
        if mutation != 3 {
            // A fresh independently pinned verification can observe this new pair;
            // the old handle must still reject it rather than reset its baseline.
            assert!(reopen(&fixture).is_ok());
            fixture.unchanged(protected);
        }
    }
}

#[test]
fn mutations_during_final_signer_fail_both_opening_and_retained_revalidation() {
    for reopening in [false, true] {
        for mutation in 0..5 {
            let fixture = Fixture::new();
            let mut value = published(&fixture);
            let protected = fixture.protected();
            let mut seen = 0;
            let mut check = |name, _: &Path| {
                seen += 1;
                if name == ArtifactName::Mcp {
                    match mutation {
                        0 => replace_file(&fixture.receipt()),
                        1 => {
                            let path = fixture.receipt();
                            chmod(&path, 0o600);
                            fs::write(&path, vec![b'!'; fixture.bytes.len()]).unwrap();
                            chmod(&path, 0o400);
                        }
                        2 => acl(&fixture.receipt()),
                        3 => {
                            let path = fixture.directory().join(NAMES[0].filename());
                            chmod(&path, 0o700);
                            fs::write(&path, b"changed executable bytes").unwrap();
                            chmod(&path, 0o500);
                        }
                        _ => chmod(&fixture.layout().process_lock(), 0o644),
                    }
                }
                Ok(())
            };
            let result = if reopening {
                reopen_published(
                    &fixture.owner,
                    &fixture.bytes,
                    &hash(&fixture.bytes),
                    CERTIFICATE,
                    Some(&mut check),
                )
                .map(|_| ())
            } else {
                revalidate(&mut value, Some(&mut check))
            };
            assert!(result.is_err());
            assert_eq!(seen, 3);
            fixture.unchanged(protected);
        }
    }
}

#[test]
fn signer_refusal_stops_each_fixed_check_in_both_routes() {
    for reopening in [false, true] {
        for refused in NAMES {
            let fixture = Fixture::new();
            let mut value = published(&fixture);
            let before = fingerprints(&fixture);
            let mut seen = Vec::new();
            let mut check = |name, _: &Path| {
                seen.push(name);
                if name == refused {
                    Err("fixture signer refusal")
                } else {
                    Ok(())
                }
            };
            let result = if reopening {
                reopen_published(
                    &fixture.owner,
                    &fixture.bytes,
                    &hash(&fixture.bytes),
                    CERTIFICATE,
                    Some(&mut check),
                )
                .map(|_| ())
            } else {
                revalidate(&mut value, Some(&mut check))
            };
            assert_eq!(result, Err("fixture signer refusal"));
            let count = NAMES.iter().position(|name| *name == refused).unwrap() + 1;
            assert_eq!(seen, NAMES[..count]);
            assert_eq!(before, fingerprints(&fixture));
        }
    }
}

#[test]
fn reopening_does_not_read_current_or_infer_selected_state() {
    for selection in [None, Some("malformed"), Some("matching")] {
        let fixture = Fixture::new();
        drop(published(&fixture));
        fs::remove_file(fixture.layout().current_release_selection()).unwrap();
        if let Some(target) = selection {
            symlink(
                if target == "matching" {
                    fixture.name()
                } else {
                    target.into()
                },
                fixture.layout().current_release_selection(),
            )
            .unwrap();
        }
        let before = fs::symlink_metadata(fixture.layout().current_release_selection())
            .ok()
            .map(Fingerprint::from);
        let mut value = reopen(&fixture).unwrap();
        revalidate(&mut value, Some(&mut |_, _| Ok(()))).unwrap();
        assert_eq!(
            before,
            fs::symlink_metadata(fixture.layout().current_release_selection())
                .ok()
                .map(Fingerprint::from)
        );
    }
}
