use super::*;
use crate::device_agreement::{DeviceAgreement, tests as device};
use crate::install_plan::Operation;
use crate::selection_intent::macos::preparation::{self as prep, Hooks, Point};
use crate::selection_intent::{PENDING_JOURNAL, PreparationFailure, PreparedSelection};

mod commit;
mod config;
mod device_admission;
mod initial;
mod job_admission;
mod pending;
mod retirement;
mod update;

const CANDIDATE: &str = ".selection-31313131313131313131313131313131";

fn setup(operation: Operation) -> (Fixture, Vec<u8>) {
    let (fixture, bytes) = pair();
    let current = fixture.layout().current_release_selection();
    fs::remove_file(&current).unwrap();
    if operation != Operation::InitialInstall {
        symlink(fixture.name(), current).unwrap();
    }
    (fixture, bytes)
}

async fn prepare<'held, 'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    operation: Operation,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    fault: &mut dyn FnMut(Point) -> Result<(), &'static str>,
) -> Result<PreparedSelection<'held, 'lock>, PreparationFailure> {
    prep::prepare(
        owner,
        agreement,
        operation,
        previous,
        destination,
        &mut Hooks {
            device: Some(device::Fake::good()),
            check: Some(&mut check),
            fault: Some(fault),
            nonce: Some([0x31; 16]),
        },
    )
    .await
}

fn unchanged(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    protected(fixture)
        .into_iter()
        .filter(|(path, _)| path != &fixture.layout().releases_directory())
        .collect()
}

#[tokio::test]
async fn random_names_are_bounded_and_zero_id_never_creates_entries() {
    let mut names = Vec::new();
    for nonce in [None, None, Some([0; 16])] {
        let (fixture, bytes) = setup(Operation::InitialInstall);
        let agreement = device::observe(&fixture.owner).await;
        let mut destination = open(&fixture.owner, &bytes);
        let result = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::InitialInstall,
            None,
            &mut destination,
            &mut Hooks {
                device: Some(device::Fake::good()),
                check: Some(&mut check),
                fault: None,
                nonce,
            },
        )
        .await;
        if nonce.is_some() {
            assert!(result.err().unwrap().residue_candidates().is_empty());
        } else {
            drop(result.unwrap());
            let journal =
                fs::read(fixture.layout().releases_directory().join(PENDING_JOURNAL)).unwrap();
            let wire: serde_json::Value = serde_json::from_slice(&journal).unwrap();
            let name = format!(".selection-{}", wire["transaction"].as_str().unwrap());
            assert_eq!(name.len(), 43);
            assert!(
                fs::symlink_metadata(fixture.layout().releases_directory().join(&name)).is_ok()
            );
            names.push(name);
        }
    }
    assert_ne!(names[0], names[1]);
}

#[tokio::test]
async fn changes_after_final_verification_still_prevent_a_prepared_handle() {
    for mutation in 0..5 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let parent = fixture.layout().releases_directory();
        let error = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |at| {
                if at == Point::Complete {
                    match mutation {
                        0 => {
                            let journal = parent.join(PENDING_JOURNAL);
                            chmod(&journal, 0o600);
                            fs::write(&journal, b"changed").unwrap();
                            chmod(&journal, 0o400);
                        }
                        1 => {
                            fs::rename(parent.join(CANDIDATE), parent.join("old-candidate"))
                                .unwrap();
                            symlink(digest(&bytes), parent.join(CANDIDATE)).unwrap();
                        }
                        2 => {
                            let current = fixture.layout().current_release_selection();
                            fs::rename(&current, parent.join("old-current")).unwrap();
                            symlink(fixture.name(), current).unwrap();
                        }
                        3 => chmod(&parent, 0o755),
                        _ => chmod(&fixture.layout().process_lock(), 0o644),
                    }
                }
                Ok(())
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.residue_candidates().len(), 2);
    }
}

#[tokio::test]
async fn preparation_records_actual_links_and_retains_original_pairs_for_all_operations() {
    for operation in [
        Operation::InitialInstall,
        Operation::Upgrade,
        Operation::Rollback,
    ] {
        let (fixture, bytes) = setup(operation);
        let agreement = device::observe(&fixture.owner).await;
        let before = unchanged(&fixture);
        let original = fs::symlink_metadata(fixture.layout().current_release_selection()).ok();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let mut value = prepare(
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
        let link = fs::symlink_metadata(parent.join(CANDIDATE)).unwrap();
        assert_eq!(
            fs::read_link(parent.join(CANDIDATE)).unwrap().as_os_str(),
            digest(&bytes).as_str()
        );
        let journal = fs::read(parent.join(PENDING_JOURNAL)).unwrap();
        let parsed = Intent::parse(&journal).unwrap();
        assert_eq!(parsed.canonical_bytes().unwrap(), journal);
        let wire: serde_json::Value = serde_json::from_slice(&journal).unwrap();
        assert_eq!(wire["destination"]["manifest"], digest(&bytes));
        assert_eq!(wire["destination"]["device"], link.dev() as i32);
        assert_eq!(wire["destination"]["inode"], link.ino());
        if let Some(original) = original {
            assert_eq!(wire["previous"]["manifest"], fixture.name());
            assert_eq!(wire["previous"]["device"], original.dev() as i32);
            assert_eq!(wire["previous"]["inode"], original.ino());
        } else {
            assert!(wire["previous"].is_null());
        }
        assert_eq!(
            fs::metadata(parent.join(PENDING_JOURNAL)).unwrap().mode() & 0o7777,
            0o400
        );
        for _ in 0..2 {
            prep::revalidate_for_test(&mut value, &mut check).unwrap();
        }
        drop(value);
        assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
        assert_eq!(fs::read(parent.join(PENDING_JOURNAL)).unwrap(), journal);
        check(&mut destination).unwrap();
        if operation != Operation::InitialInstall {
            check(&mut previous).unwrap();
        }
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn public_preparation_and_retained_revalidation_require_real_signers() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let agreement = device::observe(&fixture.owner).await;
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let error = intent::prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        error.reason(),
        "artifact signature does not satisfy signer policy"
    );
    assert!(error.residue_candidates().is_empty());
    assert!(
        !fixture
            .layout()
            .releases_directory()
            .join(PENDING_JOURNAL)
            .exists()
    );
    let agreement = device::observe(&fixture.owner).await;
    let mut value = prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        &mut |_| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(
        value.revalidate(),
        Err("artifact signature does not satisfy signer policy")
    );
}

#[tokio::test]
async fn collisions_of_any_kind_are_preserved_without_new_selection_entries() {
    for collision in 0..5 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let parent = fixture.layout().releases_directory();
        let path = parent.join(if collision == 0 {
            CANDIDATE
        } else {
            PENDING_JOURNAL
        });
        match collision {
            0 | 1 => fs::write(&path, b"existing evidence").unwrap(),
            2 => symlink("absent", &path).unwrap(),
            3 => fs::create_dir(&path).unwrap(),
            _ => {
                let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
        }
        let fingerprint = Fingerprint::from(fs::symlink_metadata(&path).unwrap());
        let count = fs::read_dir(&parent).unwrap().count();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let error = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .err()
        .unwrap();
        assert!(error.residue_candidates().is_empty());
        assert_eq!(fs::read_dir(parent).unwrap().count(), count);
        assert_eq!(
            Fingerprint::from(fs::symlink_metadata(path).unwrap()),
            fingerprint
        );
    }
}

#[tokio::test]
async fn wrong_operation_current_release_or_lock_never_creates_a_candidate() {
    for case in 0..5 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let other = Fixture::new();
        let parent = fixture.layout().releases_directory();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let result = match case {
            0 => {
                prepare(
                    &fixture.owner,
                    agreement,
                    Operation::InitialInstall,
                    None,
                    &mut destination,
                    &mut |_| Ok(()),
                )
                .await
            }
            1 => {
                prepare(
                    &fixture.owner,
                    agreement,
                    Operation::Upgrade,
                    None,
                    &mut destination,
                    &mut |_| Ok(()),
                )
                .await
            }
            2 => {
                prepare(
                    &other.owner,
                    agreement,
                    Operation::Upgrade,
                    Some(&mut previous),
                    &mut destination,
                    &mut |_| Ok(()),
                )
                .await
            }
            3 => {
                prepare(
                    &fixture.owner,
                    agreement,
                    Operation::Rollback,
                    Some(&mut destination),
                    &mut previous,
                    &mut |_| Ok(()),
                )
                .await
            }
            _ => {
                fs::remove_file(fixture.layout().current_release_selection()).unwrap();
                prepare(
                    &fixture.owner,
                    agreement,
                    Operation::Upgrade,
                    Some(&mut previous),
                    &mut destination,
                    &mut |_| Ok(()),
                )
                .await
            }
        };
        assert!(result.err().unwrap().residue_candidates().is_empty());
        assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_err());
        assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_err());
    }
}

#[tokio::test]
async fn every_failure_boundary_preserves_residues_and_the_original_selection() {
    for point in [
        Point::BeforeLink,
        Point::LinkCreated,
        Point::LinkSynced,
        Point::BeforeJournal,
        Point::JournalCreated,
        Point::JournalWritten,
        Point::JournalReadonly,
        Point::JournalSynced,
        Point::BeforeFinalVerify,
        Point::Complete,
    ] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let before = unchanged(&fixture);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let error = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |at| {
                if at == point {
                    Err("injected preparation stop")
                } else {
                    Ok(())
                }
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.reason(), "injected preparation stop");
        let expected = match point {
            Point::BeforeLink => 0,
            Point::LinkCreated | Point::LinkSynced | Point::BeforeJournal => 1,
            _ => 2,
        };
        assert_eq!(error.residue_candidates().len(), expected, "{point:?}");
        for name in error.residue_candidates() {
            assert!(fs::symlink_metadata(fixture.layout().releases_directory().join(name)).is_ok());
        }
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn checked_parent_transition_rejects_unrelated_entry_and_original_pair_changes() {
    for point in [Point::LinkCreated, Point::JournalCreated] {
        for mutation in 0..4 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let error = prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut |at| {
                    if at == point {
                        match mutation {
                            0 => fs::write(
                                fixture.layout().releases_directory().join("unrelated"),
                                b"unrelated",
                            )
                            .unwrap(),
                            1 => {
                                let receipt = fixture.receipt();
                                fs::rename(&receipt, receipt.with_extension("held")).unwrap();
                                fs::write(&receipt, &fixture.bytes).unwrap();
                                chmod(&receipt, 0o400);
                            }
                            2 => {
                                let artifact = fixture.directory().join(NAMES[0].filename());
                                chmod(&artifact, 0o700);
                                fs::write(&artifact, b"changed").unwrap();
                                chmod(&artifact, 0o500);
                            }
                            _ => chmod(&fixture.layout().process_lock(), 0o644),
                        }
                    }
                    Ok(())
                },
            )
            .await
            .err()
            .unwrap();
            assert!(!error.residue_candidates().is_empty());
        }
    }
}

#[tokio::test]
async fn retained_preparation_never_rebaselines_changed_files() {
    for mutation in 0..7 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let mut value = prepare(
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
        let journal = parent.join(PENDING_JOURNAL);
        match mutation {
            0 => {
                chmod(&journal, 0o600);
                fs::write(&journal, b"changed").unwrap();
                chmod(&journal, 0o400);
            }
            1 => {
                let bytes = fs::read(&journal).unwrap();
                fs::rename(&journal, journal.with_extension("held")).unwrap();
                fs::write(&journal, bytes).unwrap();
                chmod(&journal, 0o400);
            }
            2 => {
                fs::hard_link(&journal, parent.join("alias")).unwrap();
            }
            3 => {
                fs::rename(parent.join(CANDIDATE), parent.join("old-candidate")).unwrap();
                symlink(digest(&bytes), parent.join(CANDIDATE)).unwrap();
            }
            4 => {
                let current = fixture.layout().current_release_selection();
                fs::rename(&current, parent.join("old-current")).unwrap();
                symlink(fixture.name(), current).unwrap();
            }
            5 => chmod(&parent, 0o755),
            _ => {
                fs::rename(&parent, parent.with_extension("held")).unwrap();
                fs::create_dir(&parent).unwrap();
                chmod(&parent, 0o700);
            }
        }
        for _ in 0..2 {
            assert!(prep::revalidate_for_test(&mut value, &mut check).is_err());
        }
    }
}

#[tokio::test]
async fn final_signer_cannot_hide_changes_to_the_earlier_pair_or_journal() {
    for journal_change in [false, true] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let mut calls = 0;
        let mut checker = |release: &mut PublishedRelease<'_>| {
            check(release)?;
            calls += 1;
            if calls == 4 {
                let changed = if journal_change {
                    fixture.layout().releases_directory().join(PENDING_JOURNAL)
                } else {
                    fixture.receipt()
                };
                chmod(&changed, 0o600);
                fs::write(&changed, b"changed during final signer").unwrap();
                chmod(&changed, 0o400);
            }
            Ok(())
        };
        let error = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut Hooks {
                device: Some(device::Fake::good()),
                check: Some(&mut checker),
                fault: None,
                nonce: Some([0x31; 16]),
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(calls, 4);
        assert_eq!(error.residue_candidates().len(), 2);
    }
}
