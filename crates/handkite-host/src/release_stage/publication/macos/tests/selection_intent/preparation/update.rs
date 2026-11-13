use super::*;
use crate::launchd_observation::Observation as Job;
use crate::selection_intent::macos::transition;
use crate::selection_intent::{SelectionPhase as Phase, SelectionStage as Stage, UpdateOperation};
use prep::commit::{self, retirement};

#[tokio::test]
async fn update_coordinator_preserves_commit_phase_and_never_retries() {
    for operation in [Operation::Upgrade, Operation::Rollback] {
        for scenario in 0..12 {
            let (fixture, bytes) = setup(if scenario == 1 {
                Operation::InitialInstall
            } else {
                operation
            });
            let before = unchanged(&fixture)
                .into_iter()
                .filter(|(path, _)| path != &fixture.layout().current_release_selection())
                .collect();
            let original = fs::read_link(fixture.layout().current_release_selection()).ok();
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(
                &fixture.owner,
                if scenario == 2 {
                    &bytes
                } else {
                    &fixture.bytes
                },
            );
            let mut destination = open(&fixture.owner, &bytes);
            if scenario == 3 {
                fixture.owner.set_user_job_for_test(Job::Loaded);
            }
            let mut prepare_check = check;
            let mut commit_check = check;
            let mut retire_check = check;
            let mut final_check = |release: &mut PublishedRelease<'_>| {
                if scenario == 10 {
                    Err("fixture final check")
                } else {
                    check(release)
                }
            };
            let mut commit_fault = |point| {
                if (scenario == 4 && point == commit::Point::BeforeRename)
                    || (scenario == 5 && point == commit::Point::Renamed)
                {
                    Err("fixture commit")
                } else {
                    Ok(())
                }
            };
            let mut retire_fault = |point| {
                if (scenario == 7 && point == retirement::Point::BeforePreflight)
                    || (scenario == 8 && point == retirement::Point::Renamed)
                {
                    return Err("fixture retirement");
                }
                if scenario == 11 && point == retirement::Point::Complete {
                    fixture.owner.set_user_job_for_test(Job::Loaded);
                }
                Ok(())
            };
            let mut hooks = transition::Hooks {
                preparation: prep::Hooks {
                    check: Some(&mut prepare_check),
                    device: Some(device::Fake::good()),
                    nonce: Some([0x31; 16]),
                    ..prep::Hooks::production()
                },
                commit: commit::Hooks {
                    check: Some(&mut commit_check),
                    device: Some(device::Fake::good()),
                    fault: Some(&mut commit_fault),
                    rename_error: (scenario == 6).then_some((libc::EIO, false)),
                    ..commit::Hooks::production()
                },
                retirement: retirement::Hooks {
                    check: Some(&mut retire_check),
                    device: Some(device::Fake::good()),
                    fault: Some(&mut retire_fault),
                    rename_error: (scenario == 9).then_some((libc::EIO, false)),
                    ..retirement::Hooks::production()
                },
                final_check: Some(&mut final_check),
            };
            let result = transition::select(
                &fixture.owner,
                agreement,
                operation,
                Some(&mut previous),
                &mut destination,
                &mut hooks,
            )
            .await;
            if scenario == 0 {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                let (stage, phase, applied, residue) = match scenario {
                    1..=3 => (Stage::Preparation, Phase::Before, false, false),
                    4 => (Stage::Commit, Phase::Before, false, true),
                    5 => (Stage::Commit, Phase::Applied, true, true),
                    6 => (Stage::Commit, Phase::Uncertain, true, true),
                    7 => (Stage::Retirement, Phase::Before, true, true),
                    8 => (Stage::Retirement, Phase::Applied, true, true),
                    9 => (Stage::Retirement, Phase::Uncertain, true, true),
                    _ => (Stage::Verification, Phase::Applied, true, true),
                };
                assert_eq!(
                    (
                        error.stage(),
                        error.phase(),
                        error.selection_may_be_applied(),
                        error.has_residue()
                    ),
                    (stage, phase, applied, residue),
                    "{operation:?} scenario {scenario}"
                );
            }
            assert_eq!(
                hooks.commit.renames,
                usize::from(!matches!(scenario, 1..=4))
            );
            assert_eq!(
                hooks.retirement.renames,
                usize::from(matches!(scenario, 0 | 8..=11))
            );
            let selected = fs::read_link(fixture.layout().current_release_selection()).ok();
            assert_eq!(
                selected,
                if matches!(scenario, 0 | 5 | 7..=11) {
                    Some(digest(&bytes).into())
                } else {
                    original
                }
            );
            let parent = fixture.layout().releases_directory();
            assert_eq!(
                parent.join(PENDING_JOURNAL).exists(),
                matches!(scenario, 4..=7 | 9)
            );
            assert_eq!(
                parent
                    .join(".selection-completed-31313131313131313131313131313131.json")
                    .exists(),
                matches!(scenario, 0 | 8 | 10 | 11)
            );
            fixture.unchanged(before);
        }
    }
}

#[tokio::test]
async fn public_update_coordinator_requires_real_signer_policy_for_both_releases() {
    for operation in [UpdateOperation::Upgrade, UpdateOperation::Rollback] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let before = protected(&fixture);
        let error = intent::select_update(
            &fixture.owner,
            agreement,
            operation,
            &mut previous,
            &mut destination,
        )
        .await
        .unwrap_err();
        assert_eq!(error.stage(), Stage::Preparation);
        assert!(!error.selection_may_be_applied());
        assert!(!error.has_residue());
        fixture.unchanged(before);
    }
}
