//! No-PIN preparation modes. Observation never sends input; keypad preparation
//! reuses the owned settings transaction while the PIN stop remains present.

use super::owned::{Completion, complete};
use handkite_core::unlock::AttemptGate;
use handkite_host::install_config;
use handkite_host::pin_backend::{NativePasscodeBackend, PinPreparationDiagnostic};
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::unlock_state;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_runner::OwnedWdaRunner;
use handkite_host::wda_session::{
    SessionClose, close_owned_session, create_owned_session,
    with_owned_session_idle_settings_receipt,
};
use handkite_host::wda_settings::{IdleAction, Restoration};
use handkite_host::wda_transport::UsbWda;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn config_path(args: &[String]) -> Result<&Path, String> {
    mode_config_path(args, "--diagnose-preparation")
}

pub(super) fn prepare_config_path(args: &[String]) -> Result<&Path, String> {
    mode_config_path(args, "--prepare-keypad-only")
}

fn mode_config_path<'a>(args: &'a [String], expected_mode: &str) -> Result<&'a Path, String> {
    match args {
        [_, mode, flag, path] if mode == expected_mode && flag == "--config" => Ok(Path::new(path)),
        _ => Err("preparation=refused reason=invalid_arguments".into()),
    }
}

pub(super) fn run(path: &Path) -> Result<(), String> {
    run_mode(path, false)
}

pub(super) fn prepare(path: &Path) -> Result<(), String> {
    run_mode(path, true)
}

fn admitted(gate: &impl AttemptGate, prepares: bool, owns: bool) -> bool {
    owns && (!prepares || gate.is_disabled() == Ok(true))
}

fn run_mode(path: &Path, prepares: bool) -> Result<(), String> {
    let layout = ServiceLayout::current_account()
        .map_err(|_| "preparation=refused phase=setup reason=service_layout")?;
    if path != layout.config_file() {
        return Err("preparation=refused phase=setup reason=protected_config_required".into());
    }
    let owner = Arc::new(
        ServiceLock::acquire(layout)
            .map_err(|_| "preparation=refused phase=setup reason=service_ownership")?,
    );
    let config = install_config::load_protected_config(owner.layout())
        .map_err(|_| "preparation=refused phase=setup reason=protected_config_unavailable")?;
    let runner_id = config
        .wda_runner_bundle_id()
        .ok_or("preparation=refused phase=setup reason=runner_unconfigured")?;
    let binding = config.binding().clone();
    if binding.key.as_str() != unlock_state::SELECTED_DEVICE_KEY {
        return Err("preparation=refused phase=setup reason=selected_device".into());
    }
    // Observe protection, never take a mutable gate or call an unlock routine.
    let gate = Arc::new(
        unlock_state::iphone_gate()
            .map_err(|_| "preparation=refused phase=setup reason=gate_unavailable")?,
    );
    let stop_present = gate
        .is_disabled()
        .map_err(|_| "preparation=refused phase=setup reason=gate_unavailable")?;
    if prepares && !stop_present {
        return Err("preparation=refused phase=setup reason=stop_required".into());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "preparation=refused phase=setup reason=runtime")?;
    runtime.block_on(async {
        if !admitted(&*gate, prepares, owner.validate_current().is_ok()) {
            return Err("preparation=refused phase=setup reason=preparation_admission".into());
        }
        let started = Instant::now();
        let usb = PinnedUsb::new(binding.usb_id.clone());
        let runner = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120), {
            let owner = Arc::clone(&owner);
            move || owner.validate_current().is_ok()
        })
            .await
            .map_err(|error| {
                format!(
                    "preparation=refused phase=runner_start reason={error:?} elapsed_ms={}",
                    started.elapsed().as_millis()
                )
            })?;
        let runner_ms = started.elapsed().as_millis();
        let liveness = runner.liveness();
        let wda = UsbWda::new(usb);
        let started = Instant::now();
        let admit = || {
            if admitted(&*gate, prepares, !liveness.is_finished() && owner.validate_current().is_ok()) {
                Ok(())
            } else {
                Err(())
            }
        };
        let work = |worker_session: String| {
                let worker_owner = Arc::clone(&owner);
                let worker_liveness = liveness.clone();
                let worker_gate = Arc::clone(&gate);
                async move { tokio::task::spawn_blocking(move || {
                    NativePasscodeBackend::new_for_session(binding.usb_id, true, &worker_session)
                        .and_then(|backend| {
                            backend.with_input_admission(move || {
                                admitted(&*worker_gate, prepares, !worker_liveness.is_finished()
                                    && worker_owner.validate_current().is_ok())
                            })
                        })
                        .map(|backend| if prepares { backend.prepare_keypad_only() } else { backend.diagnose_preparation() })
                        .map_err(|_| "backend_initialization")
                })
                .await
                .unwrap_or(Err("worker_join")) }
        };
        let mut restoration = None;
        let mut session_ms = 0;
        let transaction = if prepares {
            with_owned_session_idle_settings_receipt(&wda, admit, work).await.map(|(session, outcome)| {
                restoration = Some(outcome.restoration);
                let report = match outcome.action {
                    IdleAction::Completed(result) => result,
                    IdleAction::NotRun(_) => Err("settings_not_run"),
                };
                (session, report)
            })
        } else {
            let session = create_owned_session(&wda, admit).await;
            session_ms = started.elapsed().as_millis();
            match session {
                Ok(session) => {
                    let outcome = work(session.clone()).await;
                    Ok((session, outcome))
                }
                Err(error) => Err(error),
            }
        };
        if prepares { session_ms = started.elapsed().as_millis(); }
        let session_receipt = match &transaction {
            Ok(_) => "observed".to_owned(),
            Err(error) => error.message().to_owned(),
        };
        let started = Instant::now();
        let completed = complete(
            transaction,
            |session| async move { close_owned_session(&wda, &session).await },
            || async {
                let alive = !runner.is_finished();
                (alive, runner.abort_and_join().await.is_ok())
            },
        )
        .await;
        // Emit only after owned cleanup; a closed output pipe cannot abandon it.
        println!("preparation=observed phase=runner_start elapsed_ms={runner_ms} stop_present={stop_present}");
        let session_phase = if prepares { "session_transaction" } else { "session_create" };
        println!("preparation={session_receipt} phase={session_phase} elapsed_ms={session_ms}");
        let result = finish(
            completed,
            owner.validate_current().is_ok(),
            started.elapsed().as_millis(),
            restoration,
        );
        if prepares {
            let stop_present = gate.is_disabled() == Ok(true);
            println!("preparation=observed phase=stop_guard stop_present={stop_present}");
            result?;
            if !stop_present { return Err("preparation=refused reason=stop_required".into()); }
            Ok(())
        } else { result }
    })
}

fn finish(
    completed: Completion<Result<PinPreparationDiagnostic, &'static str>>,
    owns: bool,
    cleanup_ms: u128,
    restoration: Option<Restoration>,
) -> Result<(), String> {
    // Keep the preparation receipt even when cleanup independently fails.
    if let Ok(Ok(report)) = &completed.outcome {
        println!(
            "preparation={} phase={} step={} elapsed_ms={} captures={} normal_locked={} empty_locked={} refusal={}",
            report.outcome,
            report.phase.as_str(),
            report.capture_step.as_str(),
            report.elapsed_ms,
            report.captures,
            report.normal_locked,
            report.empty_locked,
            report.refusal
        );
        let timings = report.timings;
        println!(
            "preparation=observed phase=capture_timings session_before_ms={} source_ms={} lock_ms={} direct_field_ms={} session_after_ms={} parse_ms={} native_ms={} wda_reported_lock={}",
            timings.session_before_ms,
            timings.source_ms,
            timings.lock_ms,
            timings.direct_field_ms,
            timings.session_after_ms,
            timings.parse_ms,
            timings.native_ms,
            report.wda_reported_lock.as_str()
        );
        if let Some(snapshot) = report.snapshot {
            println!(
                "preparation=observed phase=snapshot snapshot_ms={} screen_size={} field_seen={} field_accepted={} keys_seen={} keys_eligible={} keys_visible={} keys_enabled={} field_visible={} field_enabled={} layout_valid={} visibility_fallback={} native_locked={} wda_locked={} visible_locked={} prior_unlock={} alert={} normal_marker={}",
                snapshot.snapshot_ms,
                snapshot.screen_size,
                snapshot.field_seen,
                snapshot.field_accepted,
                snapshot.keys_seen,
                snapshot.keys_eligible,
                snapshot.keys_visible,
                snapshot.keys_enabled,
                snapshot.field_visible,
                snapshot.field_enabled,
                snapshot.layout_valid,
                snapshot.visibility_fallback,
                snapshot.native_locked,
                snapshot.wda_locked,
                snapshot.visible_locked,
                snapshot.prior_unlock,
                snapshot.alert,
                snapshot.normal_marker
            );
        }
    }
    let cleanup = completed.runner_alive
        && completed.runner_joined
        && owns
        && completed
            .close
            .is_none_or(|close| close == SessionClose::Acknowledged);
    println!(
        "preparation={} phase=cleanup elapsed_ms={} session_closed={} runner_alive={} runner_joined={} ownership={}",
        if cleanup { "observed" } else { "unknown" },
        cleanup_ms,
        completed.close == Some(SessionClose::Acknowledged),
        completed.runner_alive,
        completed.runner_joined,
        owns
    );
    if !cleanup {
        return Err("preparation=unknown phase=cleanup reason=owned_cleanup_unproven".into());
    }
    if let Some(restoration) = restoration {
        let verified = restoration == Restoration::Verified;
        println!("preparation=observed phase=settings_restore verified={verified}");
        if !verified {
            return Err("preparation=unknown reason=restoration_unproven".into());
        }
    }
    match completed.outcome {
        Ok(Ok(report)) if matches!(report.outcome, "reveal_required" | "empty_confirmed") => Ok(()),
        Ok(Ok(_)) => Err("preparation=refused reason=backend_unavailable".into()),
        Ok(Err(reason)) => Err(format!("preparation=refused reason={reason}")),
        Err(error) => Err(format!("preparation={}", error.message())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(outcome: &'static str) -> PinPreparationDiagnostic {
        PinPreparationDiagnostic {
            outcome,
            phase: handkite_host::pin_backend::PinPhase::InitialSnapshot,
            capture_step: handkite_host::pin_backend::PinCaptureStep::SourceRead,
            elapsed_ms: 1,
            captures: 1,
            normal_locked: false,
            empty_locked: false,
            snapshot: None,
            timings: Default::default(),
            wda_reported_lock: Default::default(),
            refusal: "none",
        }
    }

    #[test]
    fn keypad_mode_requires_stop_at_every_admission_without_mutating_it() {
        use handkite_core::unlock::GateError;
        use std::cell::Cell;
        struct Gate(Cell<bool>);
        impl AttemptGate for Gate {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(self.0.get())
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                panic!("no gate write")
            }
            fn clear(&mut self) -> Result<(), GateError> {
                panic!("no gate clear")
            }
        }
        let gate = Gate(Cell::new(true));
        assert!(admitted(&gate, true, true));
        assert!(!admitted(&gate, true, false));
        gate.0.set(false);
        assert!(!admitted(&gate, true, true));
        assert!(admitted(&gate, false, true));
    }

    #[test]
    fn keypad_result_requires_verified_settings_restoration() {
        for restoration in [
            Restoration::Verified,
            Restoration::NotNeeded,
            Restoration::Unproven(handkite_host::wda_settings::SettingsError::ReadUnproven),
        ] {
            let result = finish(
                Completion {
                    outcome: Ok(Ok(report("empty_confirmed"))),
                    close: Some(SessionClose::Acknowledged),
                    runner_alive: true,
                    runner_joined: true,
                },
                true,
                1,
                Some(restoration),
            );
            assert_eq!(result.is_ok(), restoration == Restoration::Verified);
        }
    }

    #[test]
    fn keypad_mode_accepts_only_its_exact_config_arguments() {
        let args = ["unlock", "--prepare-keypad-only", "--config", "fixture"]
            .map(String::from)
            .to_vec();
        assert_eq!(prepare_config_path(&args).unwrap(), Path::new("fixture"));
        assert!(config_path(&args).is_err());
        for flag in [
            "--owned-wda-session",
            "--coordinate-pin-taps",
            "--diagnose-preparation",
            "--temporary-idle-settings",
        ] {
            let mut invalid = args.clone();
            invalid.push(flag.into());
            assert!(prepare_config_path(&invalid).is_err());
        }
        for len in 0..args.len() {
            assert!(prepare_config_path(&args[..len]).is_err());
        }
    }

    #[test]
    fn preparation_result_and_cleanup_remain_independent() {
        for outcome in ["reveal_required", "empty_confirmed", "backend_unavailable"] {
            for close in [SessionClose::Acknowledged, SessionClose::Unknown] {
                for (alive, joined, owns) in [
                    (true, true, true),
                    (false, true, true),
                    (true, false, true),
                    (true, true, false),
                ] {
                    let result = finish(
                        Completion {
                            outcome: Ok(Ok(report(outcome))),
                            close: Some(close),
                            runner_alive: alive,
                            runner_joined: joined,
                        },
                        owns,
                        1,
                        None,
                    );
                    let clean = close == SessionClose::Acknowledged && alive && joined && owns;
                    assert_eq!(result.is_ok(), clean && outcome != "backend_unavailable");
                    if !clean {
                        assert_eq!(
                            result.unwrap_err(),
                            "preparation=unknown phase=cleanup reason=owned_cleanup_unproven"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn preparation_mode_accepts_only_config_and_never_unlock_options() {
        let args: Vec<String> = ["unlock", "--diagnose-preparation", "--config", "fixture"]
            .map(String::from)
            .to_vec();
        assert_eq!(config_path(&args).unwrap(), Path::new("fixture"));
        for flag in [
            "--owned-wda-session",
            "--temporary-idle-settings",
            "--coordinate-pin-taps",
            "--supervised-visibility-fallback",
            "--config",
        ] {
            let mut invalid = args.clone();
            invalid.push(flag.into());
            assert!(config_path(&invalid).is_err());
        }
        for len in 0..args.len() {
            assert!(config_path(&args[..len]).is_err());
        }
    }
}
