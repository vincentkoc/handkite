use super::*;
use crate::selection_intent::macos::pending;

async fn fixture_pending(operation: Operation, committed: bool) -> (Fixture, Vec<u8>) {
    let (fixture, bytes) = setup(operation);
    {
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = prepare(
            &fixture.owner,
            agreement,
            operation,
            (operation != Operation::InitialInstall).then_some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        if committed {
            drop(
                prep::commit::commit(
                    value,
                    &mut prep::commit::Hooks {
                        check: Some(&mut check),
                        device: Some(device::Fake::good()),
                        ..prep::commit::Hooks::production()
                    },
                )
                .await
                .unwrap(),
            );
        } else {
            drop(value);
        }
    }
    (fixture, bytes)
}

type Entry = (std::path::PathBuf, Fingerprint, Vec<u8>);

fn inventory(fixture: &Fixture) -> Vec<Entry> {
    fn visit(path: &Path, entries: &mut Vec<Entry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let bytes = if metadata.is_file() {
            fs::read(path).unwrap()
        } else if metadata.is_symlink() {
            fs::read_link(path).unwrap().as_os_str().as_bytes().to_vec()
        } else {
            Vec::new()
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), entries);
            }
        }
        entries.push((path.to_owned(), metadata.into(), bytes));
    }
    let mut entries = Vec::new();
    for path in [
        fixture.layout().releases_directory(),
        fixture.layout().config_file(),
        fixture.layout().process_lock(),
        fixture.layout().state_directory(),
    ] {
        visit(&path, &mut entries);
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[tokio::test]
async fn prepared_and_committed_journals_reopen_for_every_operation_without_writes() {
    for operation in [
        Operation::InitialInstall,
        Operation::Upgrade,
        Operation::Rollback,
    ] {
        for committed in [false, true] {
            let (fixture, bytes) = fixture_pending(operation, committed).await;
            let before = inventory(&fixture);
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let (mut value, observation) = pending::inspect_pending(
                &fixture.owner,
                (operation != Operation::InitialInstall).then_some(&mut previous),
                &mut destination,
                Some(&mut check),
            )
            .unwrap();
            let expected = if committed {
                Observation::DestinationSelection
            } else {
                Observation::PreviousSelection
            };
            assert_eq!(observation, expected);
            for _ in 0..2 {
                assert_eq!(
                    pending::reobserve_for_test(&mut value, &mut check),
                    Ok(expected)
                );
            }
            drop(value);
            assert_eq!(inventory(&fixture), before);
        }
    }
}

#[tokio::test]
async fn pending_production_entry_and_reobservation_require_real_signers() {
    let (fixture, bytes) = fixture_pending(Operation::Upgrade, true).await;
    let before = inventory(&fixture);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    assert_eq!(
        intent::inspect_pending(&fixture.owner, Some(&mut previous), &mut destination).err(),
        Some("artifact signature does not satisfy signer policy")
    );
    let (mut value, _) = pending::inspect_pending(
        &fixture.owner,
        Some(&mut previous),
        &mut destination,
        Some(&mut check),
    )
    .unwrap();
    assert_eq!(
        value.reobserve(),
        Err("artifact signature does not satisfy signer policy")
    );
    drop(value);
    assert_eq!(inventory(&fixture), before);
}

#[tokio::test]
async fn interrupted_preparation_missing_empty_and_writable_journals_refuse() {
    for point in [
        Point::BeforeJournal,
        Point::JournalCreated,
        Point::JournalWritten,
    ] {
        let (fixture, bytes) = setup(Operation::InitialInstall);
        let agreement = device::observe(&fixture.owner).await;
        let mut destination = open(&fixture.owner, &bytes);
        assert!(
            prepare(
                &fixture.owner,
                agreement,
                Operation::InitialInstall,
                None,
                &mut destination,
                &mut |at| if at == point {
                    Err("fixture interruption")
                } else {
                    Ok(())
                },
            )
            .await
            .is_err()
        );
        // Fresh release evidence avoids masking the journal refusal with parent drift.
        let mut destination = open(&fixture.owner, &bytes);
        let before = inventory(&fixture);
        assert!(
            pending::inspect_pending(
                &fixture.owner,
                None,
                &mut destination,
                Some(&mut |_| panic!("incomplete journal reached signer")),
            )
            .is_err()
        );
        assert_eq!(inventory(&fixture), before);
    }
}

#[tokio::test]
async fn pending_journal_invalid_bytes_and_unsafe_entries_refuse_before_signers() {
    for mutation in 0..13 {
        let (fixture, bytes) = fixture_pending(Operation::InitialInstall, false).await;
        let path = fixture.layout().releases_directory().join(PENDING_JOURNAL);
        let canonical = fs::read(&path).unwrap();
        match mutation {
            0..=4 => {
                let invalid = match mutation {
                    0 => Vec::new(),
                    1 => canonical[..canonical.len() - 1].to_vec(),
                    2 => [canonical, b"\n".to_vec()].concat(),
                    3 => vec![b'!'; intent::MAX_INTENT_BYTES + 1],
                    _ => String::from_utf8(canonical)
                        .unwrap()
                        .replace("\"version\":1", "\"version\":1,\"extra\":null")
                        .into_bytes(),
                };
                chmod(&path, 0o600);
                fs::write(&path, invalid).unwrap();
                chmod(&path, 0o400);
            }
            5 => chmod(&path, 0o600),
            6 => fs::hard_link(&path, path.with_extension("link")).unwrap(),
            7 => acl(&path),
            _ => {
                fs::remove_file(&path).unwrap();
                match mutation {
                    8 => symlink(CANDIDATE, &path).unwrap(),
                    9 => fs::create_dir(&path).unwrap(),
                    10 => {
                        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o400) }, 0);
                    }
                    11 => {
                        let temporary = tempfile::tempdir().unwrap();
                        let socket = temporary.path().join("s");
                        let _listener = UnixListener::bind(&socket).unwrap();
                        fs::rename(socket, &path).unwrap();
                    }
                    _ => {}
                }
            }
        }
        let mut destination = open(&fixture.owner, &bytes);
        let before = inventory(&fixture);
        assert!(
            pending::inspect_pending(
                &fixture.owner,
                None,
                &mut destination,
                Some(&mut |_| panic!("unsafe journal reached signer: {mutation}")),
            )
            .is_err(),
            "mutation {mutation}"
        );
        assert_eq!(inventory(&fixture), before);
    }
}

fn drift(fixture: &Fixture, mutation: u8) {
    let parent = fixture.layout().releases_directory();
    let path = parent.join(PENDING_JOURNAL);
    match mutation {
        0 => {
            let mut bytes = fs::read(&path).unwrap();
            let offset = bytes
                .windows(32)
                .position(|part| part == b"31313131313131313131313131313131")
                .unwrap();
            bytes[offset] = b'4';
            Intent::parse(&bytes).unwrap();
            chmod(&path, 0o600);
            fs::write(&path, bytes).unwrap();
            chmod(&path, 0o400);
        }
        1 => {
            let bytes = fs::read(&path).unwrap();
            fs::rename(&path, parent.join("old-journal")).unwrap();
            fs::write(&path, bytes).unwrap();
            chmod(&path, 0o400);
        }
        2 => chmod(&path, 0o600),
        3 => acl(&path),
        4 => fs::hard_link(&path, parent.join("journal-link")).unwrap(),
        5 => fs::remove_file(&path).unwrap(),
        6 => {
            let current = parent.join("current");
            let target = fs::read_link(&current).unwrap();
            fs::rename(&current, parent.join("old-current")).unwrap();
            symlink(target, current).unwrap();
        }
        7 => {
            let path = fixture.receipt();
            chmod(&path, 0o600);
            fs::write(&path, b"changed").unwrap();
            chmod(&path, 0o400);
        }
        8 => {
            let path = fixture.directory().join(NAMES[0].filename());
            chmod(&path, 0o700);
            fs::write(&path, b"changed").unwrap();
            chmod(&path, 0o500);
        }
        9 => chmod(&parent, 0o755),
        10 => chmod(&fixture.layout().process_lock(), 0o644),
        11 => fs::write(parent.join("unrelated"), b"changed").unwrap(),
        12 => {
            fs::rename(&parent, parent.with_extension("held")).unwrap();
            fs::create_dir(&parent).unwrap();
            chmod(&parent, 0o700);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn pending_reobservation_never_adopts_drifted_journal_or_custody() {
    for mutation in 0..13 {
        let (fixture, bytes) = fixture_pending(Operation::Upgrade, false).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let (mut value, _) = pending::inspect_pending(
            &fixture.owner,
            Some(&mut previous),
            &mut destination,
            Some(&mut check),
        )
        .unwrap();
        drift(&fixture, mutation);
        let after_drift = inventory(&fixture);
        for _ in 0..2 {
            assert!(
                pending::reobserve_for_test(&mut value, &mut |_| {
                    panic!("changed custody reached signer: {mutation}")
                })
                .is_err(),
                "mutation {mutation}"
            );
        }
        drop(value);
        assert_eq!(inventory(&fixture), after_drift);
    }
}

#[tokio::test]
async fn final_signer_cannot_hide_pending_journal_or_earlier_evidence_drift() {
    for reobserve in [false, true] {
        for mutation in 0..13 {
            let (fixture, bytes) = fixture_pending(Operation::Upgrade, true).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let mut checks = 0;
            let mut after_drift = None;
            let mut fault = |release: &mut PublishedRelease<'_>| {
                check(release)?;
                checks += 1;
                if checks == 2 {
                    drift(&fixture, mutation);
                    after_drift = Some(inventory(&fixture));
                }
                Ok(())
            };
            if reobserve {
                let (mut value, _) = pending::inspect_pending(
                    &fixture.owner,
                    Some(&mut previous),
                    &mut destination,
                    Some(&mut check),
                )
                .unwrap();
                assert!(pending::reobserve_for_test(&mut value, &mut fault).is_err());
            } else {
                assert!(
                    pending::inspect_pending(
                        &fixture.owner,
                        Some(&mut previous),
                        &mut destination,
                        Some(&mut fault),
                    )
                    .is_err()
                );
            }
            assert_eq!(checks, 2, "mutation {mutation}");
            assert_eq!(inventory(&fixture), after_drift.unwrap());
        }
    }
}

#[tokio::test]
async fn pending_claims_require_the_exact_lock_and_independently_supplied_releases() {
    for mismatch in 0..5 {
        let (fixture, bytes) = fixture_pending(Operation::Upgrade, false).await;
        let other = Fixture::new();
        let mut previous = open(
            &fixture.owner,
            if mismatch == 1 {
                &bytes
            } else {
                &fixture.bytes
            },
        );
        let mut destination = open(
            &fixture.owner,
            if mismatch == 2 {
                &fixture.bytes
            } else {
                &bytes
            },
        );
        let before = inventory(&fixture);
        let foreign_before = inventory(&other);
        let owner = if mismatch == 3 {
            &other.owner
        } else {
            &fixture.owner
        };
        if mismatch == 4 {
            let path = fixture.layout().releases_directory().join(PENDING_JOURNAL);
            let canonical = String::from_utf8(fs::read(&path).unwrap()).unwrap();
            let changed = canonical.replace(&digest(&bytes), &"ab".repeat(32));
            Intent::parse(changed.as_bytes()).unwrap();
            chmod(&path, 0o600);
            fs::write(&path, changed).unwrap();
            chmod(&path, 0o400);
        }
        let after_change = inventory(&fixture);
        assert!(
            pending::inspect_pending(
                owner,
                (mismatch != 0).then_some(&mut previous),
                &mut destination,
                Some(&mut |_| panic!("mismatched evidence reached signer")),
            )
            .is_err()
        );
        assert_eq!(inventory(&fixture), after_change);
        if mismatch != 4 {
            assert_eq!(inventory(&fixture), before);
        }
        assert_eq!(inventory(&other), foreign_before);
    }
}

#[tokio::test]
async fn pending_fresh_selection_conflicts_never_become_commit_history() {
    for mutation in 0..4 {
        let (fixture, bytes) = fixture_pending(Operation::Upgrade, false).await;
        let current = fixture.layout().current_release_selection();
        fs::rename(&current, current.with_extension("held")).unwrap();
        match mutation {
            0 => symlink(fixture.name(), &current).unwrap(),
            1 => {}
            2 => symlink("ab".repeat(32), &current).unwrap(),
            _ => symlink("../unsafe", &current).unwrap(),
        }
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let before = inventory(&fixture);
        let result = pending::inspect_pending(
            &fixture.owner,
            Some(&mut previous),
            &mut destination,
            Some(&mut check),
        );
        if mutation == 3 {
            assert_eq!(result.err(), Some("indeterminate selection conflict"));
        } else {
            let (mut value, observation) = result.unwrap();
            assert_eq!(observation, Observation::IndeterminateConflict);
            assert_eq!(
                pending::reobserve_for_test(&mut value, &mut check),
                Ok(Observation::IndeterminateConflict)
            );
        }
        assert_eq!(inventory(&fixture), before);
    }
}
