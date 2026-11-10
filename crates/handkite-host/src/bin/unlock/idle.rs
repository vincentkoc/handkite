use handkite_core::protocol::DeviceBinding;
use handkite_core::unlock::{AttemptGate, GateError, UnlockOutcome, UnlockPolicy, unlock_once};
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

pub(super) fn run(
    binding: DeviceBinding,
    gate: FileAttemptGate,
    supervised: bool,
) -> Result<(), &'static str> {
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
            let session = validated_active_session(&wda)
                .await
                .map_err(|_| "unlock_idle=refused")?;
            let worker_session = session.clone();
            let outcome = with_temporary_idle_settings(&wda, &session, || async move {
                // Keep the synchronous backend runtimes entirely on this worker.
                tokio::task::spawn_blocking(move || {
                    let mut gate = worker_gate.lock().map_err(|_| ())?;
                    let mut backend = NativePasscodeBackend::new_for_session(
                        binding.usb_id.clone(),
                        supervised,
                        &worker_session,
                    )
                    .map_err(|_| ())?;
                    unlock_once(
                        &mut backend,
                        &mut *gate,
                        &binding,
                        &binding.key,
                        &code,
                        UnlockPolicy::default(),
                    )
                    .map_err(|_| ())
                })
                .await
                .map_err(|_| ())?
            })
            .await;
            Ok::<_, &'static str>(outcome)
        })?;
        let mut gate = gate.lock().map_err(|_| "unlock_idle=unknown")?;
        let result = finish(&mut gate, outcome)?;
        println!("unlock_idle={result}");
        Ok(())
    })
}

fn admitted<G: AttemptGate, T>(
    gate: G,
    work: impl FnOnce(G) -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    if gate.is_disabled().map_err(|_| "unlock_idle=refused")? {
        return Err("unlock_idle=disabled");
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
    outcome: IdleSettingsOutcome<UnlockOutcome, ()>,
) -> Result<&'static str, &'static str> {
    if outcome.restoration != Restoration::Verified {
        return Err("unlock_idle=unknown");
    }
    match outcome.action {
        IdleAction::Completed(Ok(UnlockOutcome::CodeSubmitted)) if gate.clear_requested => {
            gate.inner.clear().map_err(|_| "unlock_idle=unknown")?;
            Ok("code_submitted")
        }
        IdleAction::Completed(Ok(UnlockOutcome::AlreadyUnlocked)) if !gate.clear_requested => {
            Ok("already_unlocked")
        }
        _ => Err("unlock_idle=unknown"),
    }
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
                            Err(())
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
            Err("unlock_idle=unknown")
        );
        assert_eq!(gate.inner.clears, 0);
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
                        action: IdleAction::Completed(result.map_err(|_| ())),
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
        assert!(gate.is_poisoned());
        let poisoned = gate.lock().err().unwrap().into_inner();
        assert!(poisoned.inner.disabled);
        assert_eq!(poisoned.inner.clears, 0);
    }
}
