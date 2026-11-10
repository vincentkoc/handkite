use super::*;
use crate::selection_intent::macos::preparation::commit::{self as transaction, Hooks, Point};
use crate::selection_intent::{CommitFailure, CommitPhase, CommittedSelection};

async fn commit<'held, 'lock>(
    value: PreparedSelection<'held, 'lock>,
    fault: &mut dyn FnMut(Point) -> Result<(), &'static str>,
    rename_error: Option<(i32, bool)>,
) -> (
    Result<CommittedSelection<'held, 'lock>, CommitFailure>,
    usize,
) {
    let mut hooks = Hooks {
        device: Some(device::Fake::good()),
        check: Some(&mut check),
        fault: Some(fault),
        rename_error,
        sync_error: false,
        renames: 0,
    };
    let result = transaction::commit(value, &mut hooks).await;
    (result, hooks.renames)
}

fn fingerprint(path: &Path) -> Fingerprint {
    fs::symlink_metadata(path).unwrap().into()
}

#[tokio::test]
async fn initial_upgrade_and_rollback_move_exact_links_and_preserve_the_journal() {
    for operation in [
        Operation::InitialInstall,
        Operation::Upgrade,
        Operation::Rollback,
    ] {
        let (fixture, bytes) = setup(operation);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            operation,
            (operation != Operation::InitialInstall).then_some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let parent = fixture.layout().releases_directory();
        let current = parent.join("current");
        let candidate = parent.join(CANDIDATE);
        let journal = parent.join(PENDING_JOURNAL);
        let destination_inode = fs::symlink_metadata(&candidate).unwrap().ino();
        let previous_inode = fs::symlink_metadata(&current).ok().map(|m| m.ino());
        let journal_before = fingerprint(&journal);
        let journal_bytes = fs::read(&journal).unwrap();
        let protected = unchanged(&fixture)
            .into_iter()
            .filter(|(p, _)| p != &current)
            .collect();
        let (result, renames) = commit(prepared, &mut |_| Ok(()), None).await;
        let mut committed = result.unwrap();
        assert_eq!(renames, 1);
        assert_eq!(
            fs::symlink_metadata(&current).unwrap().ino(),
            destination_inode
        );
        assert_eq!(fs::read_link(&current).unwrap(), Path::new(&digest(&bytes)));
        if let Some(inode) = previous_inode {
            assert_eq!(fs::symlink_metadata(&candidate).unwrap().ino(), inode);
            assert_eq!(
                fs::read_link(&candidate).unwrap(),
                Path::new(&fixture.name())
            );
        } else {
            assert!(fs::symlink_metadata(&candidate).is_err());
        }
        transaction::revalidate_for_test(&mut committed, &mut check).unwrap();
        transaction::revalidate_for_test(&mut committed, &mut check).unwrap();
        assert_eq!(fingerprint(&journal), journal_before);
        assert_eq!(fs::read(&journal).unwrap(), journal_bytes);
        drop(committed);
        assert_eq!(fingerprint(&journal), journal_before);
        assert_eq!(
            fs::symlink_metadata(&current).unwrap().ino(),
            destination_inode
        );
        check(&mut destination).unwrap();
        if previous_inode.is_some() {
            check(&mut previous).unwrap();
            assert!(fs::symlink_metadata(candidate).is_ok());
        }
        fixture.unchanged(protected);
    }
}

#[tokio::test]
async fn drift_before_preflight_never_calls_rename_or_cleans_residues() {
    for mutation in 0..10 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let parent = fixture.layout().releases_directory();
        let current = parent.join("current");
        let current_inode = fs::symlink_metadata(&current).unwrap().ino();
        let (result, renames) = commit(
            prepared,
            &mut |at| {
                if at == Point::BeforePreflight {
                    match mutation {
                        0 | 1 => {
                            let path =
                                parent.join(if mutation == 0 { "current" } else { CANDIDATE });
                            let target = fs::read_link(&path).unwrap();
                            fs::rename(&path, parent.join("replaced-link")).unwrap();
                            symlink(target, path).unwrap();
                        }
                        2 => {
                            let path = fixture.layout().config_file();
                            chmod(&path, 0o600);
                            fs::write(&path, b"{}").unwrap();
                        }
                        3 => {
                            let path = parent.join(PENDING_JOURNAL);
                            chmod(&path, 0o600);
                            fs::write(&path, b"changed").unwrap();
                            chmod(&path, 0o400);
                        }
                        4 => {
                            let path = parent.join(PENDING_JOURNAL);
                            let bytes = fs::read(&path).unwrap();
                            fs::rename(&path, parent.join("old-journal")).unwrap();
                            fs::write(&path, bytes).unwrap();
                            chmod(&path, 0o400);
                        }
                        5 => chmod(&parent, 0o755),
                        6 => chmod(&fixture.layout().process_lock(), 0o644),
                        7 => {
                            let path = fixture.directory().join(NAMES[0].filename());
                            chmod(&path, 0o700);
                            fs::write(&path, b"changed").unwrap();
                            chmod(&path, 0o500);
                        }
                        8 => {
                            fs::rename(&parent, parent.with_extension("held")).unwrap();
                            fs::create_dir(&parent).unwrap();
                            chmod(&parent, 0o700);
                        }
                        _ => {
                            let path = fixture.receipt();
                            chmod(&path, 0o600);
                            fs::write(&path, b"changed").unwrap();
                            chmod(&path, 0o400);
                        }
                    }
                }
                Ok(())
            },
            None,
        )
        .await;
        let error = result.err().unwrap();
        assert_eq!(
            error.phase(),
            CommitPhase::BeforeCommit,
            "mutation {mutation}"
        );
        assert_eq!(renames, 0);
        assert_eq!(
            error.residue_candidates(),
            [CANDIDATE, "current", PENDING_JOURNAL]
        );
        if mutation != 8 {
            assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
            assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_ok());
            if mutation != 0 {
                assert_eq!(fs::symlink_metadata(current).unwrap().ino(), current_inode);
            }
        }
    }
}

#[tokio::test]
async fn checked_delta_rejects_unrelated_changes_and_moved_link_drift() {
    for point in [Point::BeforeRename, Point::Renamed] {
        for mutation in 0..5 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let agreement = device::observe(&fixture.owner).await;
            let parent = fixture.layout().releases_directory();
            fs::write(parent.join("unrelated"), b"baseline").unwrap();
            // Open after the deliberate fixture addition to retain its parent baseline.
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let prepared = prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
            let (result, renames) = commit(
                prepared,
                &mut |at| {
                    if at == point {
                        match mutation {
                            0 => fs::write(parent.join("extra"), b"added").unwrap(),
                            1 => fs::write(parent.join("unrelated"), b"changed").unwrap(),
                            2 => fs::remove_file(parent.join("unrelated")).unwrap(),
                            3 => {
                                let path = parent.join("current");
                                let target = fs::read_link(&path).unwrap();
                                fs::rename(&path, parent.join("raced-current")).unwrap();
                                symlink(target, path).unwrap();
                            }
                            _ => {
                                let path =
                                    CString::new(parent.join("current").as_os_str().as_bytes())
                                        .unwrap();
                                assert_eq!(unsafe { libc::lchmod(path.as_ptr(), 0o700) }, 0);
                            }
                        }
                    }
                    Ok(())
                },
                None,
            )
            .await;
            let error = result.err().unwrap();
            assert_eq!(
                error.phase(),
                if point == Point::Renamed {
                    CommitPhase::AfterCommit
                } else {
                    CommitPhase::BeforeCommit
                }
            );
            assert_eq!(renames, usize::from(point == Point::Renamed));
            assert!(parent.join(PENDING_JOURNAL).is_file());
            assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
        }
    }
}

#[tokio::test]
async fn committed_inventory_rejects_in_place_sibling_drift_without_a_new_baseline() {
    for point in [
        Some(Point::BeforeSync),
        Some(Point::BeforeFinalVerify),
        Some(Point::Complete),
        None,
    ] {
        for change_mode in [false, true] {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let parent = fixture.layout().releases_directory();
            let unrelated = parent.join("unrelated");
            fs::write(&unrelated, b"baseline").unwrap();
            chmod(&unrelated, 0o600);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let prepared = prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
            let journal = fingerprint(&parent.join(PENDING_JOURNAL));
            let mutate = || {
                let parent_before = fingerprint(&parent);
                let entry_before = fs::symlink_metadata(&unrelated).unwrap();
                if change_mode {
                    chmod(&unrelated, 0o400);
                } else {
                    fs::write(&unrelated, b"modified").unwrap();
                }
                let entry_after = fs::symlink_metadata(&unrelated).unwrap();
                assert_eq!(entry_after.ino(), entry_before.ino());
                assert_eq!(entry_after.len(), entry_before.len());
                assert_eq!(fingerprint(&parent), parent_before);
            };
            let (result, renames) = commit(
                prepared,
                &mut |at| {
                    if point == Some(at) {
                        mutate();
                    }
                    Ok(())
                },
                None,
            )
            .await;
            assert_eq!(renames, 1);
            if point.is_some() {
                assert_eq!(result.err().unwrap().phase(), CommitPhase::AfterCommit);
            } else {
                let mut committed = result.unwrap();
                transaction::revalidate_for_test(&mut committed, &mut check).unwrap();
                mutate();
                for _ in 0..2 {
                    assert!(transaction::revalidate_for_test(&mut committed, &mut check).is_err());
                }
            }
            assert_eq!(
                fs::read_link(parent.join("current")).unwrap(),
                Path::new(&digest(&bytes))
            );
            assert_eq!(
                fs::read_link(parent.join(CANDIDATE)).unwrap(),
                Path::new(&fixture.name())
            );
            assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), journal);
        }
    }
}

#[tokio::test]
async fn a_raced_current_link_file_or_directory_is_preserved_without_success() {
    for operation in [Operation::InitialInstall, Operation::Upgrade] {
        for kind in 0..3 {
            let (fixture, bytes) = setup(operation);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let prepared = prepare(
                &fixture.owner,
                agreement,
                operation,
                (operation != Operation::InitialInstall).then_some(&mut previous),
                &mut destination,
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
            let parent = fixture.layout().releases_directory();
            let current = parent.join("current");
            let mut raced_inode = 0;
            let (result, renames) = commit(
                prepared,
                &mut |at| {
                    if at == Point::BeforeRenameCall {
                        if operation != Operation::InitialInstall {
                            fs::rename(&current, parent.join("original-current")).unwrap();
                        }
                        if kind == 2 {
                            symlink(fixture.name(), &current).unwrap();
                        } else if kind == 1 {
                            fs::create_dir(&current).unwrap();
                            fs::write(current.join("evidence"), b"preserved").unwrap();
                        } else {
                            fs::write(&current, b"preserved").unwrap();
                        }
                        raced_inode = fs::symlink_metadata(&current).unwrap().ino();
                    }
                    Ok(())
                },
                None,
            )
            .await;
            let error = result.err().unwrap();
            assert_eq!(renames, 1);
            if operation == Operation::InitialInstall {
                assert_eq!(error.phase(), CommitPhase::BeforeCommit);
            }
            let evidence = if fs::symlink_metadata(&current).unwrap().ino() == raced_inode {
                current
            } else {
                assert_eq!(error.phase(), CommitPhase::AfterCommit);
                parent.join(CANDIDATE)
            };
            assert_eq!(fs::symlink_metadata(&evidence).unwrap().ino(), raced_inode);
            if kind == 2 {
                assert_eq!(fs::read_link(evidence).unwrap(), Path::new(&fixture.name()));
            } else {
                assert_eq!(
                    fs::read(if kind == 1 {
                        evidence.join("evidence")
                    } else {
                        evidence
                    })
                    .unwrap(),
                    b"preserved"
                );
            }
            assert!(parent.join(PENDING_JOURNAL).is_file());
        }
    }
}

#[tokio::test]
async fn rename_refusals_and_uncertain_errors_never_retry_or_infer_a_commit() {
    for operation in [Operation::InitialInstall, Operation::Upgrade] {
        for (errno, applied, phase) in [
            (libc::EEXIST, false, CommitPhase::BeforeCommit),
            (libc::ENOENT, false, CommitPhase::BeforeCommit),
            (libc::ENOTSUP, false, CommitPhase::BeforeCommit),
            (libc::EACCES, false, CommitPhase::BeforeCommit),
            (libc::EIO, false, CommitPhase::UncertainCommit),
            (libc::EIO, true, CommitPhase::UncertainCommit),
            (libc::EINTR, false, CommitPhase::UncertainCommit),
            (libc::EINTR, true, CommitPhase::UncertainCommit),
        ] {
            let (fixture, bytes) = setup(operation);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let prepared = prepare(
                &fixture.owner,
                agreement,
                operation,
                (operation != Operation::InitialInstall).then_some(&mut previous),
                &mut destination,
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
            let parent = fixture.layout().releases_directory();
            let journal = fingerprint(&parent.join(PENDING_JOURNAL));
            let (result, renames) = commit(prepared, &mut |_| Ok(()), Some((errno, applied))).await;
            let error = result.err().unwrap();
            assert_eq!(error.phase(), phase);
            assert_eq!(renames, 1);
            assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), journal);
            if applied {
                assert_eq!(
                    fs::read_link(parent.join("current")).unwrap(),
                    Path::new(&digest(&bytes))
                );
            } else {
                assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
            }
        }
    }
}

#[tokio::test]
async fn every_failure_after_rename_is_after_commit_without_rollback_or_cleanup() {
    for point in [
        Point::Renamed,
        Point::BeforeSync,
        Point::BeforeFinalVerify,
        Point::Complete,
    ] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let parent = fixture.layout().releases_directory();
        let journal = fingerprint(&parent.join(PENDING_JOURNAL));
        let (result, renames) = commit(
            prepared,
            &mut |at| {
                if at == point {
                    Err("injected commit stop")
                } else {
                    Ok(())
                }
            },
            None,
        )
        .await;
        assert_eq!(result.err().unwrap().phase(), CommitPhase::AfterCommit);
        assert_eq!(renames, 1);
        assert_eq!(
            fs::read_link(parent.join("current")).unwrap(),
            Path::new(&digest(&bytes))
        );
        assert_eq!(
            fs::read_link(parent.join(CANDIDATE)).unwrap(),
            Path::new(&fixture.name())
        );
        assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), journal);
    }
}

#[tokio::test]
async fn directory_sync_failure_keeps_the_successful_swap_and_original_journal() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let agreement = device::observe(&fixture.owner).await;
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let prepared = prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        &mut |_| Ok(()),
    )
    .await
    .unwrap();
    let parent = fixture.layout().releases_directory();
    let journal = fingerprint(&parent.join(PENDING_JOURNAL));
    let mut hooks = Hooks {
        device: Some(device::Fake::good()),
        check: Some(&mut check),
        sync_error: true,
        ..Hooks::production()
    };
    let error = transaction::commit(prepared, &mut hooks)
        .await
        .err()
        .unwrap();
    assert_eq!(error.reason(), "selection parent sync failed");
    assert_eq!(error.phase(), CommitPhase::AfterCommit);
    assert_eq!(hooks.renames, 1);
    assert_eq!(
        fs::read_link(parent.join("current")).unwrap(),
        Path::new(&digest(&bytes))
    );
    assert_eq!(
        fs::read_link(parent.join(CANDIDATE)).unwrap(),
        Path::new(&fixture.name())
    );
    assert_eq!(fingerprint(&parent.join(PENDING_JOURNAL)), journal);
}

#[tokio::test]
async fn postcommit_signer_failure_and_final_drift_prevent_success() {
    for mutation in 0..6 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let parent = fixture.layout().releases_directory();
        if mutation == 5 {
            fs::write(parent.join("unrelated"), b"baseline").unwrap();
        }
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let mut calls = 0;
        let mut checker = |release: &mut PublishedRelease<'_>| {
            check(release)?;
            calls += 1;
            if calls == 4 {
                match mutation {
                    0 => return Err("injected final signer refusal"),
                    1..=3 => {
                        let path = match mutation {
                            1 => fixture.layout().config_file(),
                            2 => parent.join(PENDING_JOURNAL),
                            _ => fixture.receipt(),
                        };
                        chmod(&path, 0o600);
                        fs::write(&path, b"changed").unwrap();
                        if mutation != 1 {
                            chmod(&path, 0o400);
                        }
                    }
                    4 => {
                        fs::rename(parent.join(CANDIDATE), parent.join("old-candidate")).unwrap();
                        symlink(fixture.name(), parent.join(CANDIDATE)).unwrap();
                    }
                    _ => {
                        let before = fingerprint(&parent);
                        fs::write(parent.join("unrelated"), b"modified").unwrap();
                        assert_eq!(fingerprint(&parent), before);
                    }
                }
            }
            Ok(())
        };
        let mut hooks = Hooks {
            device: Some(device::Fake::good()),
            check: Some(&mut checker),
            ..Hooks::production()
        };
        let error = transaction::commit(prepared, &mut hooks)
            .await
            .err()
            .unwrap();
        assert_eq!(error.phase(), CommitPhase::AfterCommit);
        assert_eq!(hooks.renames, 1);
        assert_eq!(calls, 4);
        assert_eq!(
            fs::read_link(parent.join("current")).unwrap(),
            Path::new(&digest(&bytes))
        );
        assert!(parent.join(PENDING_JOURNAL).is_file());
    }
}

#[tokio::test]
async fn production_commit_and_committed_revalidation_still_require_real_signers() {
    for public_commit in [true, false] {
        let (fixture, bytes) = setup(Operation::InitialInstall);
        let agreement = device::observe(&fixture.owner).await;
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::InitialInstall,
            None,
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        if public_commit {
            let error = intent::commit(prepared).await.err().unwrap();
            assert_eq!(error.phase(), CommitPhase::BeforeCommit);
            assert_eq!(
                error.reason(),
                "artifact signature does not satisfy signer policy"
            );
            assert!(fs::symlink_metadata(fixture.layout().current_release_selection()).is_err());
        } else {
            let (result, _) = commit(prepared, &mut |_| Ok(()), None).await;
            assert_eq!(
                result.unwrap().revalidate(),
                Err("artifact signature does not satisfy signer policy")
            );
        }
    }
}

#[tokio::test]
async fn committed_revalidation_keeps_its_original_links_journal_and_config() {
    for mutation in 0..5 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let (result, _) = commit(prepared, &mut |_| Ok(()), None).await;
        let mut value = result.unwrap();
        let parent = fixture.layout().releases_directory();
        let path = match mutation {
            0 => parent.join("current"),
            1 => parent.join(CANDIDATE),
            2 => parent.join(PENDING_JOURNAL),
            3 => fixture.layout().config_file(),
            _ => fixture.directory().join(NAMES[0].filename()),
        };
        if mutation < 2 {
            let target = fs::read_link(&path).unwrap();
            fs::rename(&path, parent.join("old-link")).unwrap();
            symlink(target, path).unwrap();
        } else {
            chmod(&path, 0o600);
            fs::write(&path, b"changed").unwrap();
        }
        assert!(transaction::revalidate_for_test(&mut value, &mut check).is_err());
        assert!(transaction::revalidate_for_test(&mut value, &mut check).is_err());
    }
}
