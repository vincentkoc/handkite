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
