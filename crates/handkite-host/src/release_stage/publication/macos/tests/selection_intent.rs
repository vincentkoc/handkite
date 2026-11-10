use super::*;
use crate::selection_intent::{self as intent, Intent, Observation};

mod preparation;

fn pair() -> (Fixture, Vec<u8>) {
    let fixture = Fixture::new();
    drop(run(fixture.stage(), |_, _| Ok(())).unwrap());
    let source = fixture.source.stage.join(NAMES[0].filename());
    chmod(&source, 0o700);
    fs::write(&source, b"second fixture executable bytes").unwrap();
    chmod(&source, 0o500);
    let destination = manifest(&fixture.source.stage);
    drop(
        run(
            staged(&fixture.owner, &fixture.source.stage, &destination),
            |_, _| Ok(()),
        )
        .unwrap(),
    );
    (fixture, destination)
}

fn digest(bytes: &[u8]) -> String {
    String::from_utf8(encode_target(&hash(bytes)).to_vec()).unwrap()
}

fn open<'lock>(owner: &'lock InstallerLock, bytes: &[u8]) -> PublishedRelease<'lock> {
    reopen_published(
        owner,
        bytes,
        &hash(bytes),
        CERTIFICATE,
        Some(&mut |_, _| Ok(())),
    )
    .unwrap()
}

fn check(release: &mut PublishedRelease<'_>) -> Result<(), &'static str> {
    revalidate(release, Some(&mut |_, _| Ok(())))
}

fn claim(manifest: &str, path: &Path) -> String {
    let metadata = fs::symlink_metadata(path).unwrap();
    format!(
        "{{\"manifest\":\"{manifest}\",\"device\":{},\"inode\":{}}}",
        metadata.dev() as i32,
        metadata.ino()
    )
}

// Fixtures create both link identities before choosing the observed state.
fn record(fixture: &Fixture, destination: &[u8], operation: &str, selected: u8) -> Intent {
    let directory = fixture.layout().releases_directory();
    let previous_link = directory.join(".fixture-previous");
    let destination_link = directory.join(".fixture-destination");
    symlink(fixture.name(), &previous_link).unwrap();
    symlink(digest(destination), &destination_link).unwrap();
    let previous_claim = claim(&fixture.name(), &previous_link);
    let destination_claim = claim(&digest(destination), &destination_link);
    match selected {
        0 => fs::remove_file(fixture.layout().current_release_selection()).unwrap(),
        1 => fs::rename(previous_link, fixture.layout().current_release_selection()).unwrap(),
        2 => fs::rename(
            destination_link,
            fixture.layout().current_release_selection(),
        )
        .unwrap(),
        _ => {}
    }
    let previous = if operation == "initial-install" {
        "null"
    } else {
        &previous_claim
    };
    Intent::parse(format!("{{\"version\":1,\"transaction\":\"{}\",\"operation\":\"{operation}\",\"previous\":{previous},\"destination\":{destination_claim}}}", "12".repeat(16)).as_bytes()).unwrap()
}

fn protected(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    let mut paths = vec![
        fixture.layout().config_file(),
        fixture.layout().state_directory().join("UNLOCK-DISABLED"),
        fixture.layout().state_directory().join("state.json"),
        fixture.layout().releases_directory(),
    ];
    if fs::symlink_metadata(fixture.layout().current_release_selection()).is_ok() {
        paths.push(fixture.layout().current_release_selection());
    }
    paths
        .into_iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(&path).unwrap().into();
            (path, metadata)
        })
        .collect()
}

#[test]
fn observed_previous_destination_and_conflict_are_present_evidence_only() {
    for operation in ["initial-install", "upgrade", "rollback"] {
        for selected in 0..4 {
            let (fixture, destination) = pair();
            let record = record(&fixture, &destination, operation, selected);
            let before = protected(&fixture);
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &destination);
            let previous = if operation == "initial-install" {
                None
            } else {
                Some(&mut previous)
            };
            let (mut value, observed) = intent::macos::inspect(
                &fixture.owner,
                &record,
                previous,
                &mut destination,
                Some(&mut check),
            )
            .unwrap();
            let expected = match (operation, selected) {
                ("initial-install", 0) => Observation::PreviousSelection,
                (_, 2) => Observation::DestinationSelection,
                ("upgrade" | "rollback", 1) => Observation::PreviousSelection,
                _ => Observation::IndeterminateConflict,
            };
            assert_eq!(observed, expected);
            assert_eq!(
                intent::macos::reobserve_for_test(&mut value, &mut check),
                Ok(expected)
            );
            fixture.unchanged(before);
            if selected == 0 {
                assert!(
                    fs::symlink_metadata(fixture.layout().current_release_selection()).is_err()
                );
            }
        }
    }
}

#[test]
fn public_inspection_and_reobservation_always_recheck_the_real_signer() {
    let (fixture, destination) = pair();
    let record = record(&fixture, &destination, "upgrade", 1);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &destination);
    assert_eq!(
        intent::inspect(
            &fixture.owner,
            &record,
            Some(&mut previous),
            &mut destination
        )
        .err(),
        Some("artifact signature does not satisfy signer policy")
    );
    let (mut value, _) = intent::macos::inspect(
        &fixture.owner,
        &record,
        Some(&mut previous),
        &mut destination,
        Some(&mut check),
    )
    .unwrap();
    assert_eq!(
        value.reobserve(),
        Err("artifact signature does not satisfy signer policy")
    );
}

#[test]
fn actual_release_handles_and_same_lock_cannot_be_replaced_by_intent_claims() {
    let (fixture, destination) = pair();
    let record = record(&fixture, &destination, "upgrade", 1);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &destination);
    let other = Fixture::new();
    for case in 0..3 {
        let result = match case {
            0 => intent::macos::inspect(
                &fixture.owner,
                &record,
                None,
                &mut destination,
                Some(&mut check),
            ),
            1 => intent::macos::inspect(
                &fixture.owner,
                &record,
                Some(&mut destination),
                &mut previous,
                Some(&mut check),
            ),
            _ => intent::macos::inspect(
                &other.owner,
                &record,
                Some(&mut previous),
                &mut destination,
                Some(&mut check),
            ),
        };
        assert_eq!(
            result.err(),
            Some("selection intent release or lock mismatch")
        );
    }
}

#[test]
fn same_target_new_inode_is_conflict_and_malformed_current_has_no_inspection() {
    for malformed in [false, true] {
        let (fixture, destination) = pair();
        let record = record(&fixture, &destination, "upgrade", 1);
        let current = fixture.layout().current_release_selection();
        fs::rename(&current, current.with_extension("original")).unwrap();
        symlink(
            if malformed {
                "invalid".into()
            } else {
                fixture.name()
            },
            &current,
        )
        .unwrap();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &destination);
        let result = intent::macos::inspect(
            &fixture.owner,
            &record,
            Some(&mut previous),
            &mut destination,
            Some(&mut check),
        );
        if malformed {
            assert_eq!(result.err(), Some("indeterminate selection conflict"));
        } else {
            assert_eq!(result.unwrap().1, Observation::IndeterminateConflict);
        }
    }
}

#[test]
fn retained_snapshot_rejects_same_target_replacement_without_rebaselining() {
    let (fixture, destination) = pair();
    let record = record(&fixture, &destination, "upgrade", 1);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &destination);
    let (mut value, _) = intent::macos::inspect(
        &fixture.owner,
        &record,
        Some(&mut previous),
        &mut destination,
        Some(&mut check),
    )
    .unwrap();
    let current = fixture.layout().current_release_selection();
    fs::rename(&current, current.with_extension("original")).unwrap();
    symlink(fixture.name(), current).unwrap();
    for _ in 0..2 {
        assert!(intent::macos::reobserve_for_test(&mut value, &mut check).is_err());
    }
}

#[test]
fn final_custody_checks_reject_prior_pair_mutation_during_destination_signer() {
    let (fixture, destination) = pair();
    let record = record(&fixture, &destination, "upgrade", 1);
    let before = protected(&fixture);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &destination);
    let mut checks = 0;
    let result = intent::macos::inspect(
        &fixture.owner,
        &record,
        Some(&mut previous),
        &mut destination,
        Some(&mut |release| {
            check(release)?;
            checks += 1;
            if checks == 2 {
                chmod(&fixture.receipt(), 0o600);
                fs::write(fixture.receipt(), vec![b'!'; fixture.bytes.len()]).unwrap();
                chmod(&fixture.receipt(), 0o400);
            }
            Ok(())
        }),
    );
    assert!(result.is_err());
    assert_eq!(checks, 2);
    fixture.unchanged(before);
}

#[test]
fn selection_or_lock_changes_during_verification_never_produce_an_inspection() {
    for change_selection in [false, true] {
        let (fixture, destination) = pair();
        let record = record(&fixture, &destination, "upgrade", 1);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &destination);
        let mut checks = 0;
        let result = intent::macos::inspect(
            &fixture.owner,
            &record,
            Some(&mut previous),
            &mut destination,
            Some(&mut |release| {
                check(release)?;
                checks += 1;
                if checks == 2 {
                    if change_selection {
                        let current = fixture.layout().current_release_selection();
                        fs::rename(&current, current.with_extension("original")).unwrap();
                        symlink(fixture.name(), current).unwrap();
                    } else {
                        chmod(&fixture.layout().process_lock(), 0o644);
                    }
                }
                Ok(())
            }),
        );
        assert!(result.is_err());
        assert_eq!(checks, 2);
    }
}
