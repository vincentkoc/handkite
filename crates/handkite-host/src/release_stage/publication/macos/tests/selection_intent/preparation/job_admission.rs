use super::*;
use crate::launchd_observation::Observation as Job;
use crate::selection_intent::macos::preparation::commit::{self as transaction, retirement};
use crate::selection_intent::{CommitPhase, RetirementPhase};

const REFUSED: &str = "Handkite user-domain job is loaded or unknown";
const OPERATIONS: [Operation; 3] = [
    Operation::InitialInstall,
    Operation::Upgrade,
    Operation::Rollback,
];

fn inventory(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    let mut before = protected(fixture);
    before.extend(
        fs::read_dir(fixture.layout().releases_directory())
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let fingerprint = fs::symlink_metadata(&path).unwrap().into();
                (path, fingerprint)
            }),
    );
    before
}

fn assert_preserved(fixture: &Fixture, before: Vec<(std::path::PathBuf, Fingerprint)>) {
    fixture.unchanged(before);
    assert_eq!(fs::read(fixture.layout().config_file()).unwrap(), CONFIG);
    assert_eq!(
        fs::read(fixture.layout().state_directory().join("UNLOCK-DISABLED")).unwrap(),
        b"preserved-private-state"
    );
}

#[tokio::test]
async fn loaded_or_unknown_job_prevents_preparation_without_residues() {
    for operation in OPERATIONS {
        for job in [Job::Loaded, Job::Unknown] {
            let (fixture, bytes) = setup(operation);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let before = inventory(&fixture);
            fixture.owner.set_user_job_for_test(job);
            let error = prepare(
                &fixture.owner,
                agreement,
                operation,
                (operation != Operation::InitialInstall).then_some(&mut previous),
                &mut destination,
                &mut |_| panic!("job refusal reached preparation mutation"),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.reason(), REFUSED);
            assert!(error.residue_candidates().is_empty());
            assert_preserved(&fixture, before);
        }
    }
}

#[tokio::test]
async fn job_appearing_after_preparation_prevents_commit_and_preserves_candidates() {
    for operation in OPERATIONS {
        for job in [Job::Loaded, Job::Unknown] {
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
            let before = inventory(&fixture);
            fixture.owner.set_user_job_for_test(job);
            let mut hooks = transaction::Hooks {
                check: Some(&mut check),
                device: Some(device::Fake::good()),
                ..transaction::Hooks::production()
            };
            let error = transaction::commit(prepared, &mut hooks)
                .await
                .err()
                .unwrap();
            assert_eq!(error.reason(), REFUSED);
            assert_eq!(error.phase(), CommitPhase::BeforeCommit);
            assert_eq!(hooks.renames, 0);
            assert_preserved(&fixture, before);
        }
    }
}

#[tokio::test]
async fn job_appearing_after_commit_prevents_retirement_and_preserves_journal() {
    for operation in OPERATIONS {
        for job in [Job::Loaded, Job::Unknown] {
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
            let committed = transaction::commit(
                prepared,
                &mut transaction::Hooks {
                    check: Some(&mut check),
                    device: Some(device::Fake::good()),
                    ..transaction::Hooks::production()
                },
            )
            .await
            .unwrap();
            let before = inventory(&fixture);
            fixture.owner.set_user_job_for_test(job);
            let mut hooks = retirement::Hooks {
                check: Some(&mut check),
                device: Some(device::Fake::good()),
                ..retirement::Hooks::production()
            };
            let error = retirement::retire(committed, &mut hooks)
                .await
                .err()
                .unwrap();
            assert_eq!(error.reason(), REFUSED);
            assert_eq!(error.phase(), RetirementPhase::Before);
            assert_eq!(hooks.renames, 0);
            assert_preserved(&fixture, before);
        }
    }
}
