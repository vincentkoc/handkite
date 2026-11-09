//! Pure installation model. Caller-supplied observations are not production authority.
//!
//! Nothing here reads files, verifies evidence, holds ownership, executes candidates,
//! or changes a service. An executor and durable selection protocol do not yet exist.

pub type ManifestDigest = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    InitialInstall,
    Upgrade,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    Verified,
    Missing,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Stopped,
    Running,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Absent,
    Release(ManifestDigest),
    Unknown,
}

/// An assertion of independently pinned inventory, signer, and final-byte checks.
/// Constructing this value performs none of those checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseObservation {
    pub manifest: ManifestDigest,
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub operation: Operation,
    pub selected: Selection,
    pub destination: ReleaseObservation,
    /// Upgrade: the selected release. Rollback: the explicitly requested destination.
    pub rollback: Option<ReleaseObservation>,
    pub job: Activity,
    pub controller: Activity,
    pub layout: Evidence,
    pub configuration: Evidence,
    /// The existing configuration and independently observed device must agree.
    pub device_binding: Evidence,
    pub staged_release: Evidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    Layout,
    Configuration,
    DeviceBinding,
    StagedRelease,
    DestinationRelease,
    RollbackRelease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    MissingEvidence(Requirement),
    RejectedEvidence(Requirement),
    JobNotStopped,
    ControllerNotStopped,
    UnknownSelection,
    AlreadyInstalled,
    NotInstalled,
    SameRelease,
    UnexpectedRollback,
    RollbackMismatch,
    SelectionChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    PreserveConfigurationPairingSigningAndStopMarkers,
    RevalidateEvidenceAndOwnership,
    SelectDestination,
    VerifySelectionAndLeaveStopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    previous: Selection,
    destination: ManifestDigest,
}

impl Plan {
    /// A fixed description with one selection commit point, never executable commands.
    pub fn steps(&self) -> [Step; 4] {
        [
            Step::PreserveConfigurationPairingSigningAndStopMarkers,
            Step::RevalidateEvidenceAndOwnership,
            Step::SelectDestination,
            Step::VerifySelectionAndLeaveStopped,
        ]
    }

    pub fn destination(&self) -> ManifestDigest {
        self.destination
    }

    /// Classify a freshly observed selection after a crash. This never resumes,
    /// starts, or rolls back anything; all evidence must be collected again.
    pub fn recover(
        &self,
        selected: Selection,
        job: Activity,
        controller: Activity,
    ) -> Result<Recovery, Refusal> {
        require_stopped(job, controller)?;
        match selected {
            Selection::Unknown => Err(Refusal::UnknownSelection),
            selection if selection == self.previous => Ok(Recovery::BeforeSelection),
            Selection::Release(manifest) if manifest == self.destination => {
                Ok(Recovery::AfterSelection)
            }
            _ => Err(Refusal::SelectionChanged),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Keep the previous selection (or absence); revalidate before a new plan.
    BeforeSelection,
    /// Keep the destination selected and stopped; revalidate before any further action.
    AfterSelection,
}

pub fn plan(request: Request) -> Result<Plan, Refusal> {
    require_stopped(request.job, request.controller)?;
    for (requirement, evidence) in [
        (Requirement::Layout, request.layout),
        (Requirement::Configuration, request.configuration),
        (Requirement::DeviceBinding, request.device_binding),
        (Requirement::StagedRelease, request.staged_release),
        (
            Requirement::DestinationRelease,
            request.destination.evidence,
        ),
    ] {
        require_evidence(requirement, evidence)?;
    }
    match (request.operation, request.selected) {
        (_, Selection::Unknown) => return Err(Refusal::UnknownSelection),
        (Operation::InitialInstall, Selection::Release(_)) => {
            return Err(Refusal::AlreadyInstalled);
        }
        (Operation::InitialInstall, Selection::Absent) => {
            if request.rollback.is_some() {
                return Err(Refusal::UnexpectedRollback);
            }
        }
        (_, Selection::Absent) => return Err(Refusal::NotInstalled),
        (operation, Selection::Release(previous)) => {
            if previous == request.destination.manifest {
                return Err(Refusal::SameRelease);
            }
            let rollback = request
                .rollback
                .ok_or(Refusal::MissingEvidence(Requirement::RollbackRelease))?;
            require_evidence(Requirement::RollbackRelease, rollback.evidence)?;
            let expected = match operation {
                Operation::Upgrade => previous,
                Operation::Rollback => request.destination.manifest,
                Operation::InitialInstall => unreachable!(),
            };
            if rollback.manifest != expected {
                return Err(Refusal::RollbackMismatch);
            }
        }
    }
    Ok(Plan {
        previous: request.selected,
        destination: request.destination.manifest,
    })
}

fn require_stopped(job: Activity, controller: Activity) -> Result<(), Refusal> {
    if job != Activity::Stopped {
        return Err(Refusal::JobNotStopped);
    }
    if controller != Activity::Stopped {
        return Err(Refusal::ControllerNotStopped);
    }
    Ok(())
}

fn require_evidence(requirement: Requirement, evidence: Evidence) -> Result<(), Refusal> {
    match evidence {
        Evidence::Verified => Ok(()),
        Evidence::Missing => Err(Refusal::MissingEvidence(requirement)),
        Evidence::Rejected => Err(Refusal::RejectedEvidence(requirement)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREVIOUS: ManifestDigest = [1; 32];
    const DESTINATION: ManifestDigest = [2; 32];
    const OTHER: ManifestDigest = [3; 32];

    fn release(manifest: ManifestDigest) -> ReleaseObservation {
        ReleaseObservation {
            manifest,
            evidence: Evidence::Verified,
        }
    }

    fn request(operation: Operation) -> Request {
        Request {
            operation,
            selected: if operation == Operation::InitialInstall {
                Selection::Absent
            } else {
                Selection::Release(PREVIOUS)
            },
            destination: release(DESTINATION),
            rollback: match operation {
                Operation::InitialInstall => None,
                Operation::Upgrade => Some(release(PREVIOUS)),
                Operation::Rollback => Some(release(DESTINATION)),
            },
            job: Activity::Stopped,
            controller: Activity::Stopped,
            layout: Evidence::Verified,
            configuration: Evidence::Verified,
            device_binding: Evidence::Verified,
            staged_release: Evidence::Verified,
        }
    }

    const OPERATIONS: [Operation; 3] = [
        Operation::InitialInstall,
        Operation::Upgrade,
        Operation::Rollback,
    ];

    #[test]
    fn all_operations_preserve_state_and_have_one_selection_commit_point() {
        for operation in OPERATIONS {
            let plan = plan(request(operation)).unwrap();
            assert_eq!(plan.destination(), DESTINATION);
            assert_eq!(
                plan.steps(),
                [
                    Step::PreserveConfigurationPairingSigningAndStopMarkers,
                    Step::RevalidateEvidenceAndOwnership,
                    Step::SelectDestination,
                    Step::VerifySelectionAndLeaveStopped,
                ]
            );
        }
    }

    #[test]
    fn every_required_observation_fails_closed() {
        for operation in OPERATIONS {
            for requirement in [
                Requirement::Layout,
                Requirement::Configuration,
                Requirement::DeviceBinding,
                Requirement::StagedRelease,
                Requirement::DestinationRelease,
                Requirement::RollbackRelease,
            ] {
                if operation == Operation::InitialInstall
                    && requirement == Requirement::RollbackRelease
                {
                    continue;
                }
                for evidence in [Evidence::Missing, Evidence::Rejected] {
                    let mut input = request(operation);
                    *match requirement {
                        Requirement::Layout => &mut input.layout,
                        Requirement::Configuration => &mut input.configuration,
                        Requirement::DeviceBinding => &mut input.device_binding,
                        Requirement::StagedRelease => &mut input.staged_release,
                        Requirement::DestinationRelease => &mut input.destination.evidence,
                        Requirement::RollbackRelease => {
                            &mut input.rollback.as_mut().unwrap().evidence
                        }
                    } = evidence;
                    let expected = match evidence {
                        Evidence::Missing => Refusal::MissingEvidence(requirement),
                        Evidence::Rejected => Refusal::RejectedEvidence(requirement),
                        Evidence::Verified => unreachable!(),
                    };
                    assert_eq!(plan(input), Err(expected));
                }
            }
        }
    }

    #[test]
    fn job_and_controller_must_both_be_known_stopped() {
        for operation in OPERATIONS {
            for activity in [Activity::Running, Activity::Unknown] {
                let input = request(operation);
                let accepted = plan(input).unwrap();
                assert_eq!(
                    plan(Request {
                        job: activity,
                        ..input
                    }),
                    Err(Refusal::JobNotStopped)
                );
                assert_eq!(
                    plan(Request {
                        controller: activity,
                        ..input
                    }),
                    Err(Refusal::ControllerNotStopped)
                );
                for selection in [input.selected, Selection::Release(DESTINATION)] {
                    assert_eq!(
                        accepted.recover(selection, activity, Activity::Stopped),
                        Err(Refusal::JobNotStopped)
                    );
                    assert_eq!(
                        accepted.recover(selection, Activity::Stopped, activity),
                        Err(Refusal::ControllerNotStopped)
                    );
                }
            }
        }
    }

    #[test]
    fn operation_requires_matching_existing_selection() {
        for operation in OPERATIONS {
            let input = request(operation);
            assert_eq!(
                plan(Request {
                    selected: Selection::Unknown,
                    ..input
                }),
                Err(Refusal::UnknownSelection)
            );
            if operation == Operation::InitialInstall {
                assert_eq!(
                    plan(Request {
                        selected: Selection::Release(PREVIOUS),
                        ..input
                    }),
                    Err(Refusal::AlreadyInstalled)
                );
                assert_eq!(
                    plan(Request {
                        rollback: Some(release(PREVIOUS)),
                        ..input
                    }),
                    Err(Refusal::UnexpectedRollback)
                );
            } else {
                assert_eq!(
                    plan(Request {
                        selected: Selection::Absent,
                        ..input
                    }),
                    Err(Refusal::NotInstalled)
                );
                assert_eq!(
                    plan(Request {
                        selected: Selection::Release(DESTINATION),
                        ..input
                    }),
                    Err(Refusal::SameRelease)
                );
                assert_eq!(
                    plan(Request {
                        rollback: None,
                        ..input
                    }),
                    Err(Refusal::MissingEvidence(Requirement::RollbackRelease))
                );
                assert_eq!(
                    plan(Request {
                        rollback: Some(release(OTHER)),
                        ..input
                    }),
                    Err(Refusal::RollbackMismatch)
                );
            }
        }
    }

    #[test]
    fn crash_recovery_requires_an_observed_previous_or_destination_selection() {
        for operation in OPERATIONS {
            let input = request(operation);
            let plan = plan(input).unwrap();
            for (selection, expected) in [
                (input.selected, Ok(Recovery::BeforeSelection)),
                (
                    Selection::Release(DESTINATION),
                    Ok(Recovery::AfterSelection),
                ),
                (Selection::Release(OTHER), Err(Refusal::SelectionChanged)),
                (Selection::Unknown, Err(Refusal::UnknownSelection)),
            ] {
                assert_eq!(
                    plan.recover(selection, Activity::Stopped, Activity::Stopped),
                    expected
                );
            }
            if operation != Operation::InitialInstall {
                assert_eq!(
                    plan.recover(Selection::Absent, Activity::Stopped, Activity::Stopped),
                    Err(Refusal::SelectionChanged)
                );
            }
        }
    }
}
