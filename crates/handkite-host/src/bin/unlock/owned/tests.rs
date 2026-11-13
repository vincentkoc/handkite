use super::*;
use handkite_core::unlock::{GateError, UnlockError};
use handkite_host::file_gate::FileAttemptGate;
use handkite_host::wda_settings::{IdleAction, Restoration, SettingsError};
use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;

struct Gate(Result<bool, GateError>);

impl AttemptGate for Gate {
    fn is_disabled(&self) -> Result<bool, GateError> {
        self.0
    }
    fn mark_uncertain(&mut self) -> Result<(), GateError> {
        self.0 = Ok(true);
        Ok(())
    }
    fn clear(&mut self) -> Result<(), GateError> {
        panic!("admission cannot clear a marker")
    }
}

#[test]
fn blocked_ownership_or_gate_never_reads_secret_input() {
    for owns in [false, true] {
        for disabled in [Ok(false), Ok(true), Err(GateError)] {
            let reads = Cell::new(0);
            let result = admitted_input(
                &Gate(disabled),
                || owns,
                || {
                    reads.set(reads.get() + 1);
                    Ok(())
                },
            );
            let admitted = owns && disabled == Ok(false);
            assert_eq!(result.is_ok(), admitted);
            assert_eq!(reads.get(), usize::from(admitted));
        }
    }
}

#[tokio::test]
async fn ambiguous_or_refused_creation_never_deletes_and_always_joins() {
    for error in [
        OwnedSessionError::AdmissionRefused,
        OwnedSessionError::PreflightUnproven,
        OwnedSessionError::CreationUnknown,
        OwnedSessionError::SessionUnproven,
    ] {
        let joins = Cell::new(0);
        let completed = complete::<Outcome, _, _>(
            Err(error),
            |_| async { panic!("no exact session receipt means no DELETE") },
            || async {
                joins.set(joins.get() + 1);
                (true, true)
            },
        )
        .await;
        assert_eq!(joins.get(), 1);
        assert!(completed.close.is_none());
        assert_eq!(
            checked_outcome(completed, true).err(),
            Some(format!("unlock_idle={}", error.message()))
        );
    }
}

#[tokio::test]
async fn worker_failure_and_uncertain_restoration_still_close_once_before_join() {
    for restoration in [
        Restoration::Verified,
        Restoration::Unproven(SettingsError::WriteUnproven),
    ] {
        for close in [
            SessionClose::Acknowledged,
            SessionClose::Refused,
            SessionClose::Unknown,
        ] {
            let events = RefCell::new(Vec::new());
            let worker = tokio::task::spawn_blocking(|| Err(WorkerError::Join));
            let action = worker.await.unwrap();
            events.borrow_mut().push("worker_joined");
            events.borrow_mut().push("restoration_finished");
            let completed = complete(
                Ok((
                    "exact-owned-session".into(),
                    IdleSettingsOutcome {
                        action: IdleAction::Completed(action),
                        restoration,
                    },
                )),
                |session| {
                    assert_eq!(session, "exact-owned-session");
                    events.borrow_mut().push("close_once");
                    async move { close }
                },
                || async {
                    events.borrow_mut().push("runner_joined");
                    (true, true)
                },
            )
            .await;
            assert_eq!(
                *events.borrow(),
                [
                    "worker_joined",
                    "restoration_finished",
                    "close_once",
                    "runner_joined"
                ]
            );
            assert!(checked_outcome(completed, true).is_err());
        }
    }
}

#[test]
fn only_confirmed_submission_and_full_cleanup_clear_the_owned_durable_marker() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    for restoration in [
        Restoration::Verified,
        Restoration::NotNeeded,
        Restoration::Unproven(SettingsError::ReadUnproven),
    ] {
        for close in [
            None,
            Some(SessionClose::Acknowledged),
            Some(SessionClose::Refused),
            Some(SessionClose::Unknown),
        ] {
            for runner_alive in [false, true] {
                for runner_joined in [false, true] {
                    for owns in [false, true] {
                        for submitted in [false, true] {
                            let marker = directory.path().join("UNLOCK-DISABLED");
                            let mut gate =
                                DeferredGate::new(FileAttemptGate::new(marker.clone()).unwrap());
                            gate.mark_uncertain().unwrap();
                            if submitted {
                                gate.clear().unwrap();
                            }
                            assert!(gate.is_disabled().unwrap());
                            let completed = Completion {
                                outcome: Ok(IdleSettingsOutcome {
                                    action: IdleAction::Completed(if submitted {
                                        Ok(UnlockOutcome::CodeSubmitted)
                                    } else {
                                        Err(WorkerError::Unlock(UnlockError::Incomplete {
                                            digit: 1,
                                            progress:
                                                handkite_core::unlock::IncompleteProgress::Unchanged,
                                        }))
                                    }),
                                    restoration,
                                }),
                                close,
                                runner_alive,
                                runner_joined,
                            };
                            let result = checked_outcome(completed, owns)
                                .and_then(|outcome| idle::finish(&mut gate, outcome));
                            let success = submitted
                                && restoration == Restoration::Verified
                                && close == Some(SessionClose::Acknowledged)
                                && runner_alive
                                && runner_joined
                                && owns;
                            assert_eq!(result.is_ok(), success);
                            assert_eq!(marker.exists(), !success);
                            if !success {
                                assert_eq!(fs::read(&marker).unwrap(), b"uncertain\n");
                                fs::remove_file(marker).unwrap();
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn already_unlocked_and_unrequested_clear_do_not_clear_a_gate() {
    for outcome in [UnlockOutcome::AlreadyUnlocked, UnlockOutcome::CodeSubmitted] {
        let mut gate = DeferredGate::new(Gate(Ok(false)));
        let completed = Completion {
            outcome: Ok(IdleSettingsOutcome {
                action: IdleAction::Completed(Ok(outcome)),
                restoration: Restoration::Verified,
            }),
            close: Some(SessionClose::Acknowledged),
            runner_alive: true,
            runner_joined: true,
        };
        let result =
            checked_outcome(completed, true).and_then(|outcome| idle::finish(&mut gate, outcome));
        assert_eq!(result.is_ok(), outcome == UnlockOutcome::AlreadyUnlocked);
    }
}

#[test]
fn cleanup_failures_report_only_fixed_categories_and_bounded_position() {
    let completed = Completion {
        outcome: Ok(IdleSettingsOutcome {
            action: IdleAction::Completed(Err(WorkerError::Unlock(UnlockError::Incomplete {
                digit: 1,
                progress: handkite_core::unlock::IncompleteProgress::Unchanged,
            }))),
            restoration: Restoration::Verified,
        }),
        close: Some(SessionClose::Unknown),
        runner_alive: true,
        runner_joined: true,
    };
    assert_eq!(
        checked_outcome(completed, true).err(),
        Some("unlock_idle=unknown reason=owned_cleanup_unproven action=incomplete restoration=verified first_unconfirmed_index=1 progress=unchanged".into())
    );
    let completed = Completion {
        outcome: Err(OwnedSessionError::CreationUnknown),
        close: None,
        runner_alive: true,
        runner_joined: false,
    };
    assert_eq!(
        checked_outcome(completed, true).err(),
        Some("unlock_idle=unknown reason=owned_cleanup_unproven".into())
    );
}
