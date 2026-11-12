//! No-PIN/no-input preparation observation. Session lifecycle is explicit;
//! no idle-setting writes, gate mutations, credential reads or input methods.

use super::owned::{Completion, complete};
use handkite_core::unlock::AttemptGate;
use handkite_host::install_config;
use handkite_host::pin_backend::{NativePasscodeBackend, PinPreparationDiagnostic};
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::unlock_state;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_runner::OwnedWdaRunner;
use handkite_host::wda_session::{SessionClose, close_owned_session, create_owned_session};
use handkite_host::wda_transport::UsbWda;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn config_path(args: &[String]) -> Result<&Path, String> {
    match args {
        [_, mode, flag, path] if mode == "--diagnose-preparation" && flag == "--config" => {
            Ok(Path::new(path))
        }
        _ => Err("preparation=refused reason=invalid_arguments".into()),
    }
}

pub(super) fn run(path: &Path) -> Result<(), String> {
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
    let stop_present = unlock_state::iphone_gate()
        .and_then(|gate| gate.is_disabled())
        .map_err(|_| "preparation=refused phase=setup reason=gate_unavailable")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "preparation=refused phase=setup reason=runtime")?;
    runtime.block_on(async {
        owner
            .validate_current()
            .map_err(|_| "preparation=refused phase=setup reason=service_ownership")?;
        let started = Instant::now();
        let usb = PinnedUsb::new(binding.usb_id.clone());
        let runner = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120))
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
        let session = create_owned_session(&wda, || {
            if !liveness.is_finished() && owner.validate_current().is_ok() {
                Ok(())
            } else {
                Err(())
            }
        })
        .await;
        let session_ms = started.elapsed().as_millis();
        let session_receipt = match &session {
            Ok(_) => "observed".to_owned(),
            Err(error) => error.message().to_owned(),
        };
        let transaction = match session {
            Ok(session) => {
                let worker_owner = Arc::clone(&owner);
                let worker_liveness = liveness.clone();
                let worker_session = session.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    NativePasscodeBackend::new_for_session(binding.usb_id, true, &worker_session)
                        .and_then(|backend| {
                            backend.with_input_admission(move || {
                                !worker_liveness.is_finished()
                                    && worker_owner.validate_current().is_ok()
                            })
                        })
                        .map(NativePasscodeBackend::diagnose_preparation)
                        .map_err(|_| "backend_initialization")
                })
                .await
                .unwrap_or(Err("worker_join"));
                Ok((session, outcome))
            }
            Err(error) => Err(error),
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
        println!("preparation={session_receipt} phase=session_create elapsed_ms={session_ms}");
        finish(
            completed,
            owner.validate_current().is_ok(),
            started.elapsed().as_millis(),
        )
    })
}

fn finish(
    completed: Completion<Result<PinPreparationDiagnostic, &'static str>>,
    owns: bool,
    cleanup_ms: u128,
) -> Result<(), String> {
    // Keep the preparation receipt even when cleanup independently fails.
    if let Ok(Ok(report)) = &completed.outcome {
        println!(
            "preparation={} phase={} step={} elapsed_ms={} captures={} normal_locked={} empty_locked={}",
            report.outcome,
            report.phase.as_str(),
            report.capture_step.as_str(),
            report.elapsed_ms,
            report.captures,
            report.normal_locked,
            report.empty_locked
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
