use super::*;

fn replace(path: &Path, held: &Path, kind: usize) -> u64 {
    if fs::symlink_metadata(path).is_ok() {
        fs::rename(path, held).unwrap();
    }
    match kind {
        0 => fs::write(path, b"preserved replacement").unwrap(),
        1 => symlink("preserved-target", path).unwrap(),
        _ => {
            fs::create_dir(path).unwrap();
            fs::write(path.join("evidence"), b"preserved replacement").unwrap();
        }
    }
    fs::symlink_metadata(path).unwrap().ino()
}

fn replacement_preserved(path: &Path, kind: usize, inode: u64) {
    assert_eq!(fs::symlink_metadata(path).unwrap().ino(), inode);
    if kind == 1 {
        assert_eq!(fs::read_link(path).unwrap(), Path::new("preserved-target"));
    } else {
        assert_eq!(
            fs::read(if kind == 2 {
                path.join("evidence")
            } else {
                path.into()
            })
            .unwrap(),
            b"preserved replacement"
        );
    }
}

#[tokio::test]
async fn syscall_time_source_replacements_are_preserved_and_never_accepted() {
    for kind in 0..3 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let parent = fixture.layout().releases_directory();
        let source = parent.join(PENDING_JOURNAL);
        let original = fingerprint(&source);
        let held = parent.join("held-journal");
        let mut replacement = 0;
        let selected = fingerprint(&parent.join("current"));
        let candidate = fingerprint(&parent.join(CANDIDATE));
        let (result, renames) = retire(
            value,
            &mut |point| {
                if point == RetirementPoint::BeforeRenameCall {
                    replacement = replace(&source, &held, kind);
                }
                Ok(())
            },
            None,
        )
        .await;
        assert_eq!(renames, 1);
        assert_eq!(result.err().unwrap().phase(), RetirementPhase::Applied);
        assert!(fingerprint(&held).matches_after_rename(&original));
        replacement_preserved(&parent.join(COMPLETED), kind, replacement);
        assert!(fs::symlink_metadata(&source).is_err());
        assert_eq!(fingerprint(&parent.join("current")), selected);
        assert_eq!(fingerprint(&parent.join(CANDIDATE)), candidate);
    }
}

#[tokio::test]
async fn completion_collisions_never_overwrite_at_preflight_or_syscall() {
    for point in [
        RetirementPoint::BeforePreflight,
        RetirementPoint::BeforeRenameCall,
    ] {
        for kind in 0..3 {
            let (fixture, bytes) = setup(Operation::InitialInstall);
            let mut destination = open(&fixture.owner, &bytes);
            let value = committed(
                &fixture.owner,
                Operation::InitialInstall,
                None,
                &mut destination,
                0x31,
            )
            .await;
            let parent = fixture.layout().releases_directory();
            let pending = fingerprint(&parent.join(PENDING_JOURNAL));
            let mut replacement = 0;
            let (result, renames) = retire(
                value,
                &mut |at| {
                    if at == point {
                        replacement =
                            replace(&parent.join(COMPLETED), &parent.join("unused"), kind);
                    }
                    Ok(())
                },
                None,
            )
            .await;
            let error = result.err().unwrap();
            assert_eq!(error.phase(), RetirementPhase::Before);
            assert_eq!(
                error.residue_candidates(),
                [PENDING_JOURNAL, "current", CANDIDATE, COMPLETED]
            );
            assert_eq!(
                renames,
                usize::from(point == RetirementPoint::BeforeRenameCall)
            );
            assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), pending);
            replacement_preserved(&parent.join(COMPLETED), kind, replacement);
        }
    }
}

#[tokio::test]
async fn refusal_and_uncertain_rename_errors_never_retry_or_infer_success() {
    for (errno, applied, phase) in [
        (libc::EEXIST, false, RetirementPhase::Before),
        (libc::ENOENT, false, RetirementPhase::Before),
        (libc::ENOTSUP, false, RetirementPhase::Before),
        (libc::EACCES, false, RetirementPhase::Before),
        (libc::EIO, false, RetirementPhase::Uncertain),
        (libc::EIO, true, RetirementPhase::Uncertain),
        (libc::EINTR, false, RetirementPhase::Uncertain),
        (libc::EINTR, true, RetirementPhase::Uncertain),
    ] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let parent = fixture.layout().releases_directory();
        let pending = fingerprint(&parent.join(PENDING_JOURNAL));
        let selected = fingerprint(&parent.join("current"));
        let candidate = fingerprint(&parent.join(CANDIDATE));
        let (result, renames) = retire(value, &mut |_| Ok(()), Some((errno, applied))).await;
        assert_eq!(result.err().unwrap().phase(), phase);
        assert_eq!(renames, 1);
        if applied {
            assert!(fingerprint(&parent.join(COMPLETED)).matches_after_rename(&pending));
            assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_err());
        } else {
            assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), pending);
            assert!(fs::symlink_metadata(parent.join(COMPLETED)).is_err());
        }
        assert_eq!(fingerprint(&parent.join("current")), selected);
        assert_eq!(fingerprint(&parent.join(CANDIDATE)), candidate);
    }
}

pub(super) fn drift(fixture: &Fixture, mutation: usize, retired: bool) -> impl FnMut() + 'static {
    let parent = fixture.layout().releases_directory();
    let config = fixture.layout().config_file();
    let lock = fixture.layout().process_lock();
    let artifact = fixture.directory().join(NAMES[0].filename());
    let receipt = fixture.receipt();
    move || match mutation {
        0 | 1 => {
            let path = parent.join(if mutation == 0 { "current" } else { CANDIDATE });
            let target = fs::read_link(&path).unwrap();
            fs::rename(&path, parent.join("held-link")).unwrap();
            symlink(target, path).unwrap();
        }
        2 | 3 => {
            let path = parent.join(if retired { COMPLETED } else { PENDING_JOURNAL });
            if mutation == 2 {
                chmod(&path, 0o600);
                fs::write(&path, b"changed").unwrap();
                chmod(&path, 0o400);
            } else {
                let bytes = fs::read(&path).unwrap();
                fs::rename(&path, parent.join("held-journal")).unwrap();
                fs::write(&path, bytes).unwrap();
                chmod(&path, 0o400);
            }
        }
        4 => fs::write(&config, [CONFIG, b" "].concat()).unwrap(),
        5 => chmod(&lock, 0o644),
        6 => {
            chmod(&artifact, 0o700);
            fs::write(&artifact, b"changed").unwrap();
            chmod(&artifact, 0o500);
        }
        7 => {
            chmod(&receipt, 0o600);
            fs::write(&receipt, b"changed").unwrap();
            chmod(&receipt, 0o400);
        }
        8 => {
            let before = fingerprint(&parent);
            fs::write(parent.join("unrelated"), b"modified").unwrap();
            assert_eq!(fingerprint(&parent), before);
        }
        9 => chmod(&parent, 0o755),
        10 => fs::write(parent.join("extra"), b"unexpected").unwrap(),
        11 => fs::remove_file(parent.join("unrelated")).unwrap(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn drift_before_and_after_the_rename_never_adopts_a_replacement_baseline() {
    for point in [
        RetirementPoint::BeforePreflight,
        RetirementPoint::BeforeRename,
        RetirementPoint::Renamed,
        RetirementPoint::BeforeFinalVerify,
        RetirementPoint::Complete,
    ] {
        for mutation in 0..12 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let parent = fixture.layout().releases_directory();
            fs::write(parent.join("unrelated"), b"baseline").unwrap();
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let value = committed(
                &fixture.owner,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                0x31,
            )
            .await;
            let applied = !matches!(
                point,
                RetirementPoint::BeforePreflight | RetirementPoint::BeforeRename
            );
            let mut mutate = drift(&fixture, mutation, applied);
            let (result, renames) = retire(
                value,
                &mut |at| {
                    if at == point {
                        mutate();
                    }
                    Ok(())
                },
                None,
            )
            .await;
            assert_eq!(
                result.err().unwrap().phase(),
                if applied {
                    RetirementPhase::Applied
                } else {
                    RetirementPhase::Before
                },
                "{point:?} {mutation}"
            );
            assert_eq!(renames, usize::from(applied));
            assert!(
                parent
                    .join(if applied { COMPLETED } else { PENDING_JOURNAL })
                    .is_file()
            );
        }
    }
}

#[tokio::test]
async fn sync_final_signer_and_checkpoint_failures_preserve_the_applied_move() {
    for failure in 0_usize..6 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let parent = fixture.layout().releases_directory();
        let pending = fingerprint(&parent.join(PENDING_JOURNAL));
        let mut calls = 0;
        let mut signer = |release: &mut PublishedRelease<'_>| {
            check(release)?;
            calls += 1;
            if failure == 1 && calls == 4 {
                return Err("injected final signer refusal");
            }
            Ok(())
        };
        let mut fault = |point| {
            if [
                RetirementPoint::Renamed,
                RetirementPoint::BeforeSync,
                RetirementPoint::BeforeFinalVerify,
                RetirementPoint::Complete,
            ]
            .get(failure.wrapping_sub(2))
            .is_some_and(|expected| *expected == point)
            {
                Err("injected retirement stop")
            } else {
                Ok(())
            }
        };
        let mut hooks = RetirementHooks {
            check: Some(&mut signer),
            device: Some(super::super::device::Fake::good()),
            fault: Some(&mut fault),
            sync_error: failure == 0,
            ..RetirementHooks::production()
        };
        let error = retirement::retire(value, &mut hooks).await.err().unwrap();
        assert_eq!(error.phase(), RetirementPhase::Applied);
        assert_eq!(hooks.renames, 1);
        assert!(fingerprint(&parent.join(COMPLETED)).matches_after_rename(&pending));
        assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_err());
    }
}

#[tokio::test]
async fn returned_retired_custody_rejects_later_drift_without_mutation() {
    for mutation in 0..12 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        fs::write(
            fixture.layout().releases_directory().join("unrelated"),
            b"baseline",
        )
        .unwrap();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let (result, _) = retire(value, &mut |_| Ok(()), None).await;
        let mut retired = result.unwrap();
        drift(&fixture, mutation, true)();
        let before = inventory(&fixture);
        assert!(retirement::revalidate_for_test(&mut retired, &mut check).is_err());
        drop(retired);
        fixture.unchanged(before);
    }
}
