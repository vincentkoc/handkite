use super::*;
use crate::launchd_observation::Observation as Job;
use crate::selection_intent::InitialSelectionStage as Stage;
use crate::selection_intent::macos::initial;
use prep::commit::{self, retirement};

#[tokio::test]
async fn initial_coordinator_preserves_the_global_commit_boundary() {
    for scenario in 0..9 {
        let (fixture, bytes) = setup(if scenario == 1 {
            Operation::Upgrade
        } else {
            Operation::InitialInstall
        });
        let before = unchanged(&fixture)
            .into_iter()
            .filter(|(path, _)| path != &fixture.layout().current_release_selection())
            .collect();
        let original_current = fs::read_link(fixture.layout().current_release_selection()).ok();
        let agreement = device::observe(&fixture.owner).await;
        let mut destination = open(&fixture.owner, &bytes);
        if scenario == 2 {
            fixture.owner.set_user_job_for_test(Job::Loaded);
        }
        let mut prepare_check = check;
        let mut commit_check = check;
        let mut retire_check = check;
        let mut final_check = |release: &mut PublishedRelease<'_>| {
            if scenario == 7 {
                Err("fixture final verification failure")
            } else {
                check(release)
            }
        };
        let mut commit_fault = |point| {
            if (scenario == 3 && point == commit::Point::BeforeRename)
                || (scenario == 4 && point == commit::Point::Renamed)
            {
                Err("fixture commit failure")
            } else {
                Ok(())
            }
        };
        let mut retire_fault = |point| {
            if (scenario == 5 && point == retirement::Point::BeforePreflight)
                || (scenario == 6 && point == retirement::Point::Renamed)
            {
                return Err("fixture retirement failure");
            }
            if scenario == 8 && point == retirement::Point::Complete {
                fixture.owner.set_user_job_for_test(Job::Loaded);
            }
            Ok(())
        };
        let mut hooks = initial::Hooks {
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
                ..commit::Hooks::production()
            },
            retirement: retirement::Hooks {
                check: Some(&mut retire_check),
                device: Some(device::Fake::good()),
                fault: Some(&mut retire_fault),
                ..retirement::Hooks::production()
            },
            final_check: Some(&mut final_check),
        };
        let result = initial::select(&fixture.owner, agreement, &mut destination, &mut hooks).await;
        if scenario == 0 {
            result.unwrap();
            assert_eq!(hooks.commit.renames, 1);
            assert_eq!(hooks.retirement.renames, 1);
        } else {
            let error = result.unwrap_err();
            let (stage, applied, residue) = match scenario {
                1 | 2 => (Stage::Preparation, false, false),
                3 => (Stage::Commit, false, true),
                4 => (Stage::Commit, true, true),
                5 | 6 => (Stage::Retirement, true, true),
                7 | 8 => (Stage::Verification, true, true),
                _ => unreachable!(),
            };
            assert_eq!(error.stage(), stage, "scenario {scenario}");
            assert_eq!(error.selection_may_be_applied(), applied);
            assert_eq!(error.has_residue(), residue);
            assert_eq!(hooks.commit.renames, usize::from(scenario >= 4));
            assert_eq!(hooks.retirement.renames, usize::from(scenario >= 6));
        }
        let parent = fixture.layout().releases_directory();
        let current = fs::read_link(fixture.layout().current_release_selection()).ok();
        if scenario == 0 || scenario >= 4 {
            assert_eq!(current, Some(digest(&bytes).into()));
        } else {
            assert_eq!(current, original_current);
        }
        assert_eq!(
            parent.join(PENDING_JOURNAL).exists(),
            (3..=5).contains(&scenario)
        );
        let completed = parent.join(".selection-completed-31313131313131313131313131313131.json");
        assert_eq!(completed.exists(), scenario == 0 || scenario >= 6);
        fixture.unchanged(before);
        assert_eq!(fs::read(fixture.layout().config_file()).unwrap(), CONFIG);
        assert_eq!(
            fs::read(fixture.layout().state_directory().join("UNLOCK-DISABLED")).unwrap(),
            b"preserved-private-state"
        );
    }
}

#[tokio::test]
async fn public_initial_coordinator_requires_the_real_signer_policy() {
    let (fixture, bytes) = setup(Operation::InitialInstall);
    let agreement = device::observe(&fixture.owner).await;
    let mut destination = open(&fixture.owner, &bytes);
    let before = protected(&fixture);
    let error = intent::select_initial(&fixture.owner, agreement, &mut destination)
        .await
        .unwrap_err();
    assert_eq!(error.stage(), Stage::Preparation);
    assert!(!error.selection_may_be_applied());
    assert!(!error.has_residue());
    fixture.unchanged(before);
}
