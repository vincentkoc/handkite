use handkite_core::protocol::DeviceBinding;
use handkite_core::unlock::{
    AttemptGate, BackendError, GateError, UnlockError, UnlockOutcome, UnlockPolicy, unlock_once,
};
use handkite_host::file_gate::FileAttemptGate;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::pin_input;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_session::with_owned_session_idle_settings;
use handkite_host::wda_settings::{
    IdleAction, IdleSettingsOutcome, Restoration, validated_active_session,
    with_temporary_idle_settings,
};
use handkite_host::wda_transport::UsbWda;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum WorkerError {
    Unlock(UnlockError),
    Initialization,
    GateAccess,
    Join,
}

pub(super) fn run(
    binding: DeviceBinding,
    gate: FileAttemptGate,
    supervised: bool,
    coordinate_pin_taps: bool,
    owned_session: bool,
) -> Result<(), String> {
    admitted(gate, |gate| {
        // The gate check precedes secret input and every possible settings POST.
        let code =
            pin_input::read_once(std::io::stdin().lock()).map_err(|_| "unlock_idle=refused")?;
        let gate = Arc::new(Mutex::new(DeferredGate::new(gate)));
        let worker_gate = gate.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "unlock_idle=refused")?;
        let outcome = runtime.block_on(async {
            let usb = PinnedUsb::new(binding.usb_id.clone());
            usb.provider().await.map_err(|_| "unlock_idle=refused")?;
            let wda = UsbWda::new(usb);
            let action = |worker_session: String| async move {
                // Keep the synchronous backend runtimes entirely on this worker.
                tokio::task::spawn_blocking(move || {
                    let mut gate = worker_gate.lock().map_err(|_| WorkerError::GateAccess)?;
                    let mut backend = NativePasscodeBackend::new_for_session(
                        binding.usb_id.clone(),
                        supervised,
                        &worker_session,
                    )
                    .map_err(|_| WorkerError::Initialization)?;
                    if coordinate_pin_taps {
                        backend = backend
                            .with_coordinate_pin_taps()
                            .map_err(|_| WorkerError::Initialization)?;
                    }
                    unlock_once(
                        &mut backend,
                        &mut *gate,
                        &binding,
                        &binding.key,
                        &code,
                        UnlockPolicy::default(),
                    )
                    .map_err(WorkerError::Unlock)
                })
                .await
                .map_err(|_| WorkerError::Join)?
            };
            if owned_session {
                with_owned_session_idle_settings(
                    &wda,
                    || {
                        let gate = gate.lock().map_err(|_| ())?;
                        if gate.is_disabled().map_err(|_| ())? {
                            Err(())
                        } else {
                            Ok(())
                        }
                    },
                    action,
                )
                .await
                .map_err(|error| format!("unlock_idle={}", error.message()))
            } else {
                let session = validated_active_session(&wda)
                    .await
                    .map_err(|_| "unlock_idle=refused")?;
                Ok(with_temporary_idle_settings(&wda, &session, || action(session.clone())).await)
            }
        })?;
        // Diagnose worker failure before touching a possibly poisoned gate mutex.
        if let Some(error) = failure(&outcome) {
            return Err(error);
        }
        let mut gate = gate
            .lock()
            .map_err(|_| diagnostic(&outcome, "gate_access"))?;
        let result = finish(&mut gate, outcome)?;
        println!("unlock_idle={result}");
        Ok(())
    })
}

fn admitted<G: AttemptGate, T, E: From<&'static str>>(
    gate: G,
    work: impl FnOnce(G) -> Result<T, E>,
) -> Result<T, E> {
    if gate
        .is_disabled()
        .map_err(|_| E::from("unlock_idle=refused"))?
    {
        return Err(E::from("unlock_idle=disabled"));
    }
    work(gate)
}

struct DeferredGate<G> {
    inner: G,
    clear_requested: bool,
}

impl<G> DeferredGate<G> {
    fn new(inner: G) -> Self {
        Self {
            inner,
            clear_requested: false,
        }
    }
}

impl<G: AttemptGate> AttemptGate for DeferredGate<G> {
    fn is_disabled(&self) -> Result<bool, GateError> {
        self.inner.is_disabled()
    }
    fn mark_uncertain(&mut self) -> Result<(), GateError> {
        self.inner.mark_uncertain()
    }
    fn clear(&mut self) -> Result<(), GateError> {
        self.clear_requested = true;
        Ok(())
    }
}

fn finish<G: AttemptGate>(
    gate: &mut DeferredGate<G>,
    outcome: IdleSettingsOutcome<UnlockOutcome, WorkerError>,
) -> Result<&'static str, String> {
    if let Some(error) = failure(&outcome) {
        return Err(error);
    }
    match outcome.action {
        IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)) if gate.clear_requested => {
            gate.inner
                .clear()
                .map_err(|_| diagnostic(&outcome, "gate_clear_failed"))?;
            Ok("code_submitted")
        }
        IdleAction::Completed(Ok(UnlockOutcome::AlreadyUnlocked)) if !gate.clear_requested => {
            Ok("already_unlocked")
        }
        _ => Err(diagnostic(&outcome, "clear_request_unproven")),
    }
}

fn failure(outcome: &IdleSettingsOutcome<UnlockOutcome, WorkerError>) -> Option<String> {
    let reason = if matches!(outcome.restoration, Restoration::Unproven(_))
        || (outcome.restoration == Restoration::NotNeeded
            && !matches!(outcome.action, IdleAction::NotRun(_)))
    {
        "restoration_unproven"
    } else {
        match outcome.action {
            IdleAction::Completed(Ok(_)) => return None,
            _ => action_reason(&outcome.action).0,
        }
    };
    Some(diagnostic(outcome, reason))
}

fn diagnostic(
    outcome: &IdleSettingsOutcome<UnlockOutcome, WorkerError>,
    reason: &'static str,
) -> String {
    let (action, index) = action_reason(&outcome.action);
    let restoration = match outcome.restoration {
        Restoration::Verified => "verified",
        Restoration::NotNeeded => "not_needed",
        Restoration::Unproven(_) => "unproven",
    };
    let mut line =
        format!("unlock_idle=unknown reason={reason} action={action} restoration={restoration}");
    if let Some(index) = index {
        line.push_str(&format!(" first_unconfirmed_index={index}"));
    }
    line
}

fn action_reason(action: &IdleAction<UnlockOutcome, WorkerError>) -> (&'static str, Option<u8>) {
    let reason = match action {
        IdleAction::NotRun(_) => "settings_not_run",
        IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)) => "code_submitted",
        IdleAction::Completed(Ok(UnlockOutcome::AlreadyUnlocked)) => "already_unlocked",
        IdleAction::Completed(Err(WorkerError::Initialization)) => "backend_initialization",
        IdleAction::Completed(Err(WorkerError::GateAccess)) => "gate_access",
        IdleAction::Completed(Err(WorkerError::Join)) => "worker_join",
        IdleAction::Completed(Err(WorkerError::Unlock(error))) => match error {
            UnlockError::Disabled => "disabled",
            UnlockError::DeviceMismatch => "device_mismatch",
            UnlockError::ManualRequired => "manual_required",
            UnlockError::NoKeypad => "no_keypad",
            UnlockError::UnreadableProgress => "unreadable_progress",
            UnlockError::LengthMismatch { .. } => "length_mismatch",
            UnlockError::ResidualInput => "residual_input",
            UnlockError::Incomplete {
                digit: index @ 1..=6,
            } => return ("incomplete", Some(*index as u8)),
            UnlockError::Incomplete { .. } => "incomplete_index_unproven",
            UnlockError::UnlockedBeforeLastDigit => "unlocked_before_last_digit",
            UnlockError::WrongOrUncertain => "wrong_or_uncertain",
            UnlockError::Backend(BackendError::Disconnected) => "backend_disconnected",
            UnlockError::Backend(BackendError::Unavailable) => "backend_unavailable",
            UnlockError::Backend(BackendError::ProgressMismatch) => "progress_mismatch",
            UnlockError::Gate => "gate_unproven",
        },
    };
    (reason, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_host::wda_settings::SettingsError;

    #[derive(Default)]
    struct Gate {
        disabled: bool,
        clears: usize,
    }

    impl AttemptGate for Gate {
        fn is_disabled(&self) -> Result<bool, GateError> {
            Ok(self.disabled)
        }
        fn mark_uncertain(&mut self) -> Result<(), GateError> {
            self.disabled = true;
            Ok(())
        }
        fn clear(&mut self) -> Result<(), GateError> {
            self.clears += 1;
            self.disabled = false;
            Ok(())
        }
    }

    #[test]
    fn preexisting_marker_blocks_secret_read_settings_and_backend_entry() {
        let mut secret_reads = 0;
        let mut settings_posts = 0;
        let mut inputs = 0;
        let result = admitted(
            Gate {
                disabled: true,
                clears: 0,
            },
            |_| {
                secret_reads += 1;
                settings_posts += 1;
                inputs += 1;
                Ok(())
            },
        );
        assert_eq!(result, Err("unlock_idle=disabled"));
        assert_eq!((secret_reads, settings_posts, inputs), (0, 0, 0));
    }

    #[test]
    fn typed_worker_and_unlock_failures_emit_only_allowlisted_classes() {
        let cases = [
            (WorkerError::Initialization, "backend_initialization"),
            (WorkerError::GateAccess, "gate_access"),
            (WorkerError::Join, "worker_join"),
            (WorkerError::Unlock(UnlockError::Disabled), "disabled"),
            (
                WorkerError::Unlock(UnlockError::DeviceMismatch),
                "device_mismatch",
            ),
            (
                WorkerError::Unlock(UnlockError::ManualRequired),
                "manual_required",
            ),
            (WorkerError::Unlock(UnlockError::NoKeypad), "no_keypad"),
            (
                WorkerError::Unlock(UnlockError::UnreadableProgress),
                "unreadable_progress",
            ),
            (
                WorkerError::Unlock(UnlockError::LengthMismatch {
                    observed_total: u32::MAX,
                }),
                "length_mismatch",
            ),
            (
                WorkerError::Unlock(UnlockError::ResidualInput),
                "residual_input",
            ),
            (
                WorkerError::Unlock(UnlockError::UnlockedBeforeLastDigit),
                "unlocked_before_last_digit",
            ),
            (
                WorkerError::Unlock(UnlockError::WrongOrUncertain),
                "wrong_or_uncertain",
            ),
            (
                WorkerError::Unlock(UnlockError::Backend(BackendError::Disconnected)),
                "backend_disconnected",
            ),
            (
                WorkerError::Unlock(UnlockError::Backend(BackendError::Unavailable)),
                "backend_unavailable",
            ),
            (WorkerError::Unlock(UnlockError::Gate), "gate_unproven"),
            (
                WorkerError::Unlock(UnlockError::Backend(BackendError::ProgressMismatch)),
                "progress_mismatch",
            ),
        ];
        for (error, token) in cases {
            let mut gate = DeferredGate::new(Gate::default());
            gate.mark_uncertain().unwrap();
            let result = finish(
                &mut gate,
                IdleSettingsOutcome {
                    action: IdleAction::Completed(Err(error)),
                    restoration: Restoration::Verified,
                },
            );
            assert_eq!(
                result,
                Err(format!(
                    "unlock_idle=unknown reason={token} action={token} restoration=verified"
                ))
            );
            assert!(gate.inner.disabled);
            assert_eq!(gate.inner.clears, 0);
        }
    }

    #[test]
    fn incomplete_reports_only_bounded_unconfirmed_position_and_restore_precedence() {
        for digit in [0, 1, 2, 3, 4, 5, 6, 7, u32::MAX] {
            for restoration in [
                Restoration::Verified,
                Restoration::Unproven(SettingsError::ReadUnproven),
            ] {
                let mut gate = DeferredGate::new(Gate::default());
                gate.mark_uncertain().unwrap();
                let output = finish(
                    &mut gate,
                    IdleSettingsOutcome {
                        action: IdleAction::Completed(Err(WorkerError::Unlock(
                            UnlockError::Incomplete { digit },
                        ))),
                        restoration,
                    },
                )
                .unwrap_err();
                let bounded = (1..=6).contains(&digit);
                let action = if bounded {
                    "incomplete"
                } else {
                    "incomplete_index_unproven"
                };
                let verified = restoration == Restoration::Verified;
                let reason = if verified {
                    action
                } else {
                    "restoration_unproven"
                };
                let restore = if verified { "verified" } else { "unproven" };
                let suffix = if bounded {
                    format!(" first_unconfirmed_index={digit}")
                } else {
                    String::new()
                };
                assert_eq!(
                    output,
                    format!(
                        "unlock_idle=unknown reason={reason} action={action} restoration={restore}{suffix}"
                    )
                );
                assert!(gate.inner.disabled);
                assert_eq!(gate.inner.clears, 0);
            }
        }
    }

    #[test]
    fn settings_not_run_and_unverified_restore_cannot_report_success_or_clear() {
        for restoration in [
            Restoration::NotNeeded,
            Restoration::Verified,
            Restoration::Unproven(SettingsError::WriteUnproven),
        ] {
            let mut gate = DeferredGate::new(Gate::default());
            gate.mark_uncertain().unwrap();
            let output = finish(
                &mut gate,
                IdleSettingsOutcome {
                    action: IdleAction::NotRun(SettingsError::SessionUnproven),
                    restoration,
                },
            )
            .unwrap_err();
            let (reason, restore) = match restoration {
                Restoration::Verified => ("settings_not_run", "verified"),
                Restoration::NotNeeded => ("settings_not_run", "not_needed"),
                Restoration::Unproven(_) => ("restoration_unproven", "unproven"),
            };
            assert_eq!(
                output,
                format!(
                    "unlock_idle=unknown reason={reason} action=settings_not_run restoration={restore}"
                )
            );
            assert!(gate.inner.disabled);
            assert_eq!(gate.inner.clears, 0);
        }
    }

    #[test]
    fn only_submitted_code_with_verified_restore_commits_deferred_clear() {
        for restoration in [
            Restoration::Verified,
            Restoration::NotNeeded,
            Restoration::Unproven(SettingsError::WriteUnproven),
        ] {
            for submitted in [false, true] {
                let mut gate = DeferredGate::new(Gate::default());
                gate.mark_uncertain().unwrap();
                if submitted {
                    gate.clear().unwrap();
                }
                assert!(gate.inner.disabled);
                assert_eq!(gate.inner.clears, 0);
                let result = finish(
                    &mut gate,
                    IdleSettingsOutcome {
                        action: IdleAction::Completed(if submitted {
                            Ok(UnlockOutcome::CodeSubmitted)
                        } else {
                            Err(WorkerError::Unlock(UnlockError::Incomplete { digit: 1 }))
                        }),
                        restoration,
                    },
                );
                let success = submitted && restoration == Restoration::Verified;
                assert_eq!(result.is_ok(), success);
                assert_eq!(gate.inner.clears, usize::from(success));
                assert_eq!(gate.inner.disabled, !success);
            }
        }
        let mut gate = DeferredGate::new(Gate::default());
        assert_eq!(
            finish(
                &mut gate,
                IdleSettingsOutcome {
                    action: IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)),
                    restoration: Restoration::Verified,
                }
            ),
            Err("unlock_idle=unknown reason=clear_request_unproven action=code_submitted restoration=verified".into())
        );
        assert_eq!(gate.inner.clears, 0);
    }

    #[test]
    fn gate_clear_failure_is_fixed_unknown_after_verified_restoration() {
        struct FailingClear;
        impl AttemptGate for FailingClear {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(true)
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                Ok(())
            }
            fn clear(&mut self) -> Result<(), GateError> {
                Err(GateError)
            }
        }
        let mut gate = DeferredGate::new(FailingClear);
        gate.clear().unwrap();
        assert_eq!(finish(&mut gate, IdleSettingsOutcome {
            action: IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)),
            restoration: Restoration::Verified,
        }), Err("unlock_idle=unknown reason=gate_clear_failed action=code_submitted restoration=verified".into()));
        assert!(gate.is_disabled().unwrap());
    }

    #[test]
    fn real_unlock_flow_defers_clear_and_incomplete_input_keeps_marker() {
        use handkite_core::passcode::Passcode;
        use handkite_core::protocol::{DeviceKey, HardwareId, LockState};
        use handkite_core::unlock::{BackendError, PasscodeBackend, PasscodeScreen};
        use std::time::Duration;
        struct Phone {
            presses: usize,
            incomplete: bool,
        }
        impl PasscodeBackend for Phone {
            fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
                let unlocked = self.presses == 4 && !self.incomplete;
                let state = if unlocked {
                    LockState::Unlocked
                } else {
                    LockState::Locked
                };
                Ok(PasscodeScreen {
                    connected_usb: HardwareId::new("fixture-phone").unwrap(),
                    wda_target: HardwareId::new("fixture-phone").unwrap(),
                    native_lock: state,
                    visible_lock: state,
                    wda_lock: state,
                    keypad_ready: true,
                    secure_field_value: Some(format!(
                        "{} of 4 digits",
                        if self.incomplete { 0 } else { self.presses }
                    )),
                    before_first_unlock: false,
                    trust_prompt: false,
                })
            }
            fn show_keypad(&mut self) -> Result<(), BackendError> {
                Ok(())
            }
            fn press_digit(&mut self, _: u8) -> Result<(), BackendError> {
                self.presses += 1;
                Ok(())
            }
            fn delete_digit(&mut self) -> Result<(), BackendError> {
                panic!("no residual input")
            }
        }
        let binding = DeviceBinding {
            key: DeviceKey::new("fixture").unwrap(),
            usb_id: HardwareId::new("fixture-phone").unwrap(),
        };
        for incomplete in [false, true] {
            for restoration in [
                Restoration::Verified,
                Restoration::Unproven(SettingsError::ReadUnproven),
            ] {
                let mut gate = DeferredGate::new(Gate::default());
                let mut phone = Phone {
                    presses: 0,
                    incomplete,
                };
                let code = Passcode::new("1234".into()).unwrap();
                let result = unlock_once(
                    &mut phone,
                    &mut gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy {
                        progress_polls: 0,
                        final_polls: 0,
                        poll_delay: Duration::ZERO,
                    },
                );
                assert!(gate.inner.disabled);
                assert_eq!(gate.inner.clears, 0);
                assert_eq!(phone.presses, if incomplete { 1 } else { 4 });
                let result = finish(
                    &mut gate,
                    IdleSettingsOutcome {
                        action: IdleAction::Completed(result.map_err(WorkerError::Unlock)),
                        restoration,
                    },
                );
                let success = !incomplete && restoration == Restoration::Verified;
                assert_eq!(result.is_ok(), success);
                assert_eq!(gate.inner.clears, usize::from(success));
                assert_eq!(gate.inner.disabled, !success);
            }
        }
    }

    #[tokio::test]
    async fn worker_panic_retains_marked_gate_and_cannot_clear() {
        let gate = Arc::new(Mutex::new(DeferredGate::new(Gate::default())));
        let worker = gate.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let mut gate = worker.lock().unwrap();
            gate.mark_uncertain().unwrap();
            gate.clear().unwrap();
            panic!("fixture worker panic");
        })
        .await;
        assert!(joined.is_err());
        assert_eq!(
            failure(&IdleSettingsOutcome {
                action: IdleAction::Completed(Err(WorkerError::Join)),
                restoration: Restoration::Verified,
            }),
            Some(
                "unlock_idle=unknown reason=worker_join action=worker_join restoration=verified"
                    .into()
            )
        );
        assert!(gate.is_poisoned());
        let poisoned = gate.lock().err().unwrap().into_inner();
        assert!(poisoned.inner.disabled);
        assert_eq!(poisoned.inner.clears, 0);
    }
}
