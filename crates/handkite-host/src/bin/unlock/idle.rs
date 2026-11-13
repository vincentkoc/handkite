use handkite_core::protocol::DeviceBinding;
use handkite_core::unlock::{
    AttemptGate, BackendError, GateError, IncompleteProgress, UnlockError, UnlockOutcome,
    UnlockPolicy, unlock_once,
};
use handkite_host::file_gate::FileAttemptGate;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::pin_input;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_settings::{
    IdleAction, IdleSettingsOutcome, Restoration, validated_active_session,
    with_temporary_idle_settings,
};
use handkite_host::wda_transport::UsbWda;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub(super) enum WorkerError {
    Unlock(UnlockError),
    TracedUnlock(
        UnlockError,
        handkite_host::pin_backend::PinPhase,
        handkite_host::pin_backend::PinCaptureStep,
        Option<[TimeBucket; 7]>,
    ),
    Initialization,
    GateAccess,
    Join,
}

pub(super) fn run(
    binding: DeviceBinding,
    gate: FileAttemptGate,
    supervised: bool,
    coordinate_pin_taps: bool,
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
            let session = validated_active_session(&wda)
                .await
                .map_err(|_| "unlock_idle=refused")?;
            Ok::<_, String>(
                with_temporary_idle_settings(&wda, &session, || action(session.clone())).await,
            )
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

pub(super) struct DeferredGate<G> {
    inner: G,
    clear_requested: bool,
}

impl<G> DeferredGate<G> {
    pub(super) fn new(inner: G) -> Self {
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

pub(super) fn finish<G: AttemptGate>(
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

pub(super) fn failure(outcome: &IdleSettingsOutcome<UnlockOutcome, WorkerError>) -> Option<String> {
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

pub(super) fn diagnostic(
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
    if let IdleAction::Completed(Err(WorkerError::TracedUnlock(_, phase, step, timings))) =
        outcome.action
    {
        line.push_str(&format!(" phase={} step={}", phase.as_str(), step.as_str()));
        if let Some(timings) = timings {
            for (name, bucket) in [
                "session_before_time",
                "source_time",
                "lock_time",
                "direct_field_time",
                "session_after_time",
                "parse_time",
                "native_time",
            ]
            .into_iter()
            .zip(timings)
            {
                line.push_str(&format!(" {name}={}", bucket.as_str()));
            }
        }
    }
    if let IdleAction::Completed(Err(
        WorkerError::Unlock(UnlockError::Incomplete { progress, .. })
        | WorkerError::TracedUnlock(UnlockError::Incomplete { progress, .. }, _, _, _),
    )) = outcome.action
    {
        let progress = match progress {
            IncompleteProgress::Unchanged => "unchanged",
            IncompleteProgress::Unexpected => "unexpected",
        };
        line.push_str(&format!(" progress={progress}"));
    }
    line
}

#[derive(Clone, Copy)]
pub(super) enum TimeBucket {
    Under1ms,
    Under10ms,
    Under100ms,
    Under1s,
    Under5s,
    Under10s,
    Under30s,
    Under120s,
    Over120s,
}

impl TimeBucket {
    fn as_str(self) -> &'static str {
        match self {
            Self::Under1ms => "lt_1ms",
            Self::Under10ms => "lt_10ms",
            Self::Under100ms => "lt_100ms",
            Self::Under1s => "lt_1s",
            Self::Under5s => "lt_5s",
            Self::Under10s => "lt_10s",
            Self::Under30s => "lt_30s",
            Self::Under120s => "lt_120s",
            Self::Over120s => "ge_120s",
        }
    }
}

pub(super) fn capture_timing_buckets(
    t: handkite_host::pin_backend::PinCaptureTimings,
) -> [TimeBucket; 7] {
    [
        t.session_before_ms,
        t.source_ms,
        t.lock_ms,
        t.direct_field_ms,
        t.session_after_ms,
        t.parse_ms,
        t.native_ms,
    ]
    .map(timing_bucket)
}

fn timing_bucket(elapsed_ms: u128) -> TimeBucket {
    match elapsed_ms {
        0 => TimeBucket::Under1ms,
        1..=9 => TimeBucket::Under10ms,
        10..=99 => TimeBucket::Under100ms,
        100..=999 => TimeBucket::Under1s,
        1_000..=4_999 => TimeBucket::Under5s,
        5_000..=9_999 => TimeBucket::Under10s,
        10_000..=29_999 => TimeBucket::Under30s,
        30_000..=119_999 => TimeBucket::Under120s,
        _ => TimeBucket::Over120s,
    }
}

fn action_reason(action: &IdleAction<UnlockOutcome, WorkerError>) -> (&'static str, Option<u8>) {
    let reason = match action {
        IdleAction::NotRun(_) => "settings_not_run",
        IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)) => "code_submitted",
        IdleAction::Completed(Ok(UnlockOutcome::AlreadyUnlocked)) => "already_unlocked",
        IdleAction::Completed(Err(WorkerError::Initialization)) => "backend_initialization",
        IdleAction::Completed(Err(WorkerError::GateAccess)) => "gate_access",
        IdleAction::Completed(Err(WorkerError::Join)) => "worker_join",
        IdleAction::Completed(Err(
            WorkerError::Unlock(error) | WorkerError::TracedUnlock(error, _, _, _),
        )) => match error {
            UnlockError::Disabled => "disabled",
            UnlockError::DeviceMismatch => "device_mismatch",
            UnlockError::ManualRequired => "manual_required",
            UnlockError::NoKeypad => "no_keypad",
            UnlockError::UnreadableProgress => "unreadable_progress",
            UnlockError::LengthMismatch { .. } => "length_mismatch",
            UnlockError::ResidualInput => "residual_input",
            UnlockError::Incomplete {
                digit: index @ 1..=6,
                ..
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
    fn capture_timings_preserve_failure_and_only_emit_fixed_buckets() {
        use handkite_host::pin_backend::{PinCaptureStep, PinCaptureTimings, PinPhase};
        for (elapsed, label) in [
            (0, "lt_1ms"),
            (1, "lt_10ms"),
            (9, "lt_10ms"),
            (10, "lt_100ms"),
            (99, "lt_100ms"),
            (100, "lt_1s"),
            (999, "lt_1s"),
            (1000, "lt_5s"),
            (4999, "lt_5s"),
            (5000, "lt_10s"),
            (9999, "lt_10s"),
            (10000, "lt_30s"),
            (29999, "lt_30s"),
            (30000, "lt_120s"),
            (119999, "lt_120s"),
            (120000, "ge_120s"),
            (u128::MAX, "ge_120s"),
        ] {
            assert_eq!(timing_bucket(elapsed).as_str(), label);
        }
        let outcome = IdleSettingsOutcome {
            action: IdleAction::Completed(Err(WorkerError::TracedUnlock(
                UnlockError::Backend(BackendError::Unavailable),
                PinPhase::DigitInput,
                PinCaptureStep::Freshness,
                Some(capture_timing_buckets(PinCaptureTimings {
                    source_ms: 5010,
                    native_ms: 7999,
                    ..Default::default()
                })),
            ))),
            restoration: Restoration::Verified,
        };
        let mut gate = DeferredGate::new(Gate {
            disabled: true,
            clears: 0,
        });
        let error = finish(&mut gate, outcome).unwrap_err();
        assert!(error.starts_with("unlock_idle=unknown reason=backend_unavailable action=backend_unavailable restoration=verified phase=digit_input step=freshness "));
        assert!(error.contains(" source_time=lt_10s "));
        assert!(error.ends_with(" native_time=lt_10s"));
        assert!(gate.inner.disabled);
        assert_eq!(gate.inner.clears, 0);
    }

    #[test]
    fn owned_failure_steps_distinguish_capture_expiry_from_unready_views() {
        use handkite_host::pin_backend::{PinCaptureStep, PinPhase};
        for step in [
            PinCaptureStep::SourceRead,
            PinCaptureStep::ViewportRead,
            PinCaptureStep::SessionAfter,
            PinCaptureStep::Freshness,
            PinCaptureStep::EmptyAdmission,
            PinCaptureStep::RevealSettleLimit,
        ] {
            let outcome = IdleSettingsOutcome {
                action: IdleAction::Completed(Err(WorkerError::TracedUnlock(
                    UnlockError::Backend(BackendError::Unavailable),
                    PinPhase::RevealPostcapture,
                    step,
                    None,
                ))),
                restoration: Restoration::Verified,
            };
            assert_eq!(
                diagnostic(&outcome, "backend_unavailable"),
                format!(
                    "unlock_idle=unknown reason=backend_unavailable action=backend_unavailable restoration=verified phase=reveal_postcapture step={}",
                    step.as_str()
                )
            );
        }
    }

    #[test]
    fn owned_phase_diagnostics_preserve_errors_restoration_and_stop_protection() {
        use handkite_host::pin_backend::PinPhase;
        for phase in [
            PinPhase::InitialSnapshot,
            PinPhase::OwnedAdmission,
            PinPhase::WakeDispatch,
            PinPhase::WakePostcapture,
            PinPhase::WakeAdmission,
            PinPhase::NormalConfirmation,
            PinPhase::RevealDispatch,
            PinPhase::RevealPostcapture,
            PinPhase::EmptyConfirmation,
            PinPhase::DigitInput,
        ] {
            for (error, action, index) in [
                (
                    UnlockError::Backend(BackendError::Unavailable),
                    "backend_unavailable",
                    "",
                ),
                (
                    UnlockError::Incomplete {
                        digit: 1,
                        progress: handkite_core::unlock::IncompleteProgress::Unchanged,
                    },
                    "incomplete",
                    " first_unconfirmed_index=1",
                ),
            ] {
                for restoration in [
                    Restoration::Verified,
                    Restoration::Unproven(SettingsError::ReadUnproven),
                ] {
                    let mut gate = DeferredGate::new(Gate::default());
                    gate.mark_uncertain().unwrap();
                    let reason = if restoration == Restoration::Verified {
                        action
                    } else {
                        "restoration_unproven"
                    };
                    let restore = if restoration == Restoration::Verified {
                        "verified"
                    } else {
                        "unproven"
                    };
                    let progress = if matches!(error, UnlockError::Incomplete { .. }) {
                        " progress=unchanged"
                    } else {
                        ""
                    };
                    assert_eq!(
                        finish(
                            &mut gate,
                            IdleSettingsOutcome {
                                action: IdleAction::Completed(Err(WorkerError::TracedUnlock(
                                    error,
                                    phase,
                                    handkite_host::pin_backend::PinCaptureStep::Complete,
                                    None,
                                ))),
                                restoration
                            }
                        ),
                        Err(format!(
                            "unlock_idle=unknown reason={reason} action={action} restoration={restore}{index} phase={} step=complete{progress}",
                            phase.as_str()
                        ))
                    );
                    assert!(gate.inner.disabled);
                    assert_eq!(gate.inner.clears, 0);
                }
            }
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
                            UnlockError::Incomplete {
                                digit,
                                progress: IncompleteProgress::Unchanged,
                            },
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
                        "unlock_idle=unknown reason={reason} action={action} restoration={restore}{suffix} progress=unchanged"
                    )
                );
                assert!(gate.inner.disabled);
                assert_eq!(gate.inner.clears, 0);
            }
        }
    }

    #[test]
    fn incomplete_progress_is_fixed_for_traced_and_untraced_failures() {
        use handkite_host::pin_backend::{PinCaptureStep, PinPhase};
        for (progress, label) in [
            (IncompleteProgress::Unchanged, "unchanged"),
            (IncompleteProgress::Unexpected, "unexpected"),
        ] {
            let error = UnlockError::Incomplete { digit: 1, progress };
            for traced in [false, true] {
                let error = if traced {
                    WorkerError::TracedUnlock(
                        error,
                        PinPhase::DigitInput,
                        PinCaptureStep::Complete,
                        None,
                    )
                } else {
                    WorkerError::Unlock(error)
                };
                let outcome = IdleSettingsOutcome {
                    action: IdleAction::Completed(Err(error)),
                    restoration: Restoration::Verified,
                };
                let phase = if traced {
                    " phase=digit_input step=complete"
                } else {
                    ""
                };
                assert_eq!(
                    failure(&outcome).unwrap(),
                    format!(
                        "unlock_idle=unknown reason=incomplete action=incomplete restoration=verified first_unconfirmed_index=1{phase} progress={label}"
                    )
                );
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
                            Err(WorkerError::Unlock(UnlockError::Incomplete {
                                digit: 1,
                                progress: handkite_core::unlock::IncompleteProgress::Unchanged,
                            }))
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
