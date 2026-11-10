use super::*;
use crate::selection_intent::macos::preparation::commit::{self as transaction, retirement};
use crate::selection_intent::{
    CommittedSelection, RetiredSelection, RetirementFailure, RetirementPhase,
};
use retirement::{Hooks as RetirementHooks, Point as RetirementPoint};

mod device;
mod failures;

const COMPLETED: &str = ".selection-completed-31313131313131313131313131313131.json";

async fn committed<'held, 'lock>(
    owner: &'lock InstallerLock,
    operation: Operation,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    nonce: u8,
) -> CommittedSelection<'held, 'lock> {
    let agreement = super::device::observe(owner).await;
    let prepared = prep::prepare(
        owner,
        agreement,
        operation,
        previous,
        destination,
        &mut Hooks {
            check: Some(&mut check),
            device: Some(super::device::Fake::good()),
            nonce: Some([nonce; 16]),
            ..Hooks::production()
        },
    )
    .await
    .unwrap();
    transaction::commit(
        prepared,
        &mut transaction::Hooks {
            check: Some(&mut check),
            device: Some(super::device::Fake::good()),
            ..transaction::Hooks::production()
        },
    )
    .await
    .unwrap()
}

async fn retire<'held, 'lock>(
    value: CommittedSelection<'held, 'lock>,
    fault: &mut dyn FnMut(RetirementPoint) -> Result<(), &'static str>,
    rename_error: Option<(i32, bool)>,
) -> (
    Result<RetiredSelection<'held, 'lock>, RetirementFailure>,
    usize,
) {
    let mut hooks = RetirementHooks {
        check: Some(&mut check),
        device: Some(super::device::Fake::good()),
        fault: Some(fault),
        rename_error,
        ..RetirementHooks::production()
    };
    let result = retirement::retire(value, &mut hooks).await;
    (result, hooks.renames)
}

fn fingerprint(path: &Path) -> Fingerprint {
    fs::symlink_metadata(path).unwrap().into()
}

fn inventory(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    let mut paths = protected(fixture);
    paths.extend(
        fs::read_dir(fixture.layout().releases_directory())
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let metadata = fingerprint(&path);
                (path, metadata)
            }),
    );
    paths
}

#[tokio::test]
async fn exact_retirement_preserves_selection_and_allows_a_second_transaction() {
    for operation in [
        Operation::InitialInstall,
        Operation::Upgrade,
        Operation::Rollback,
    ] {
        let (fixture, bytes) = setup(operation);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            operation,
            (operation != Operation::InitialInstall).then_some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let parent = fixture.layout().releases_directory();
        let pending = parent.join(PENDING_JOURNAL);
        let completed = parent.join(COMPLETED);
        let original = fingerprint(&pending);
        let journal_bytes = fs::read(&pending).unwrap();
        let preserved = inventory(&fixture)
            .into_iter()
            .filter(|(path, _)| path != &parent && path != &pending)
            .collect();
        let (result, renames) = retire(value, &mut |_| Ok(()), None).await;
        let mut retired = result.unwrap();
        assert_eq!(renames, 1);
        assert!(fs::symlink_metadata(&pending).is_err());
        assert!(fingerprint(&completed).matches_after_rename(&original));
        assert_eq!(fs::read(&completed).unwrap(), journal_bytes);
        retirement::revalidate_for_test(&mut retired, &mut check).unwrap();
        fixture.unchanged(preserved);
        drop(retired);
        check(&mut destination).unwrap();
        if operation != Operation::InitialInstall {
            check(&mut previous).unwrap();
        }
        let completed_before = fingerprint(&completed);
        // Reopen the original pair for initial install, where it was not borrowed.
        let mut next_destination = open(&fixture.owner, &fixture.bytes);
        let next = committed(
            &fixture.owner,
            Operation::Rollback,
            Some(&mut destination),
            &mut next_destination,
            0x32,
        )
        .await;
        let (result, renames) = retire(next, &mut |_| Ok(()), None).await;
        let mut retired = result.unwrap();
        assert_eq!(renames, 1);
        retirement::revalidate_for_test(&mut retired, &mut check).unwrap();
        assert_eq!(fingerprint(&completed), completed_before);
        assert!(
            parent
                .join(".selection-completed-32323232323232323232323232323232.json")
                .is_file()
        );
        assert!(fs::symlink_metadata(&pending).is_err());
        assert_eq!(
            fs::read_link(parent.join("current")).unwrap(),
            Path::new(&fixture.name())
        );
        if operation != Operation::InitialInstall {
            assert_eq!(
                fs::read_link(parent.join(CANDIDATE)).unwrap(),
                Path::new(&fixture.name())
            );
        }
    }
}

#[tokio::test]
async fn production_retirement_and_retired_revalidation_require_real_signers() {
    for public_retire in [true, false] {
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
        if public_retire {
            let before = inventory(&fixture);
            let error = intent::retire(value).await.err().unwrap();
            assert_eq!(error.phase(), RetirementPhase::Before);
            assert_eq!(
                error.reason(),
                "artifact signature does not satisfy signer policy"
            );
            fixture.unchanged(before);
        } else {
            let (result, _) = retire(value, &mut |_| Ok(()), None).await;
            assert_eq!(
                result.unwrap().revalidate(),
                Err("artifact signature does not satisfy signer policy")
            );
        }
    }
}

#[tokio::test]
async fn read_only_pending_inspection_and_unpolled_retirement_preserve_every_name() {
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
    let before = inventory(&fixture);
    drop(intent::retire(value));
    fixture.unchanged(before);
    let before = inventory(&fixture);
    let (mut inspection, observation) = intent::macos::pending::inspect_pending(
        &fixture.owner,
        Some(&mut previous),
        &mut destination,
        Some(&mut check),
    )
    .unwrap();
    assert_eq!(observation, intent::Observation::DestinationSelection);
    intent::macos::pending::reobserve_for_test(&mut inspection, &mut check).unwrap();
    drop(inspection);
    fixture.unchanged(before);
}
