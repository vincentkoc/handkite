use super::idle::{self, DeferredGate, WorkerError};
use handkite_core::unlock::{AttemptGate, UnlockOutcome, UnlockPolicy, unlock_once};
use handkite_host::install_config;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_runner::OwnedWdaRunner;
use handkite_host::wda_session::{
    OwnedSessionError, SessionClose, close_owned_session, with_owned_session_idle_settings_receipt,
};
use handkite_host::wda_settings::IdleSettingsOutcome;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{pin_input, unlock_state};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Outcome = IdleSettingsOutcome<UnlockOutcome, WorkerError>;

pub(super) fn run(path: &Path, supervised: bool, coordinate: bool) -> Result<(), String> {
    let layout = ServiceLayout::current_account()
        .map_err(|_| "unlock_idle=refused reason=service_layout")?;
    if path != layout.config_file() {
        return Err("unlock_idle=refused reason=protected_config_required".into());
    }
    let owner = Arc::new(
        ServiceLock::acquire(layout).map_err(|_| "unlock_idle=refused reason=service_ownership")?,
    );
    let config = install_config::load_protected_config(owner.layout())
        .map_err(|_| "unlock_idle=refused reason=protected_config_unavailable")?;
    let runner_id = config
        .wda_runner_bundle_id()
        .ok_or("unlock_idle=refused reason=runner_unconfigured")?;
    let binding = config.binding().clone();
    if binding.key.as_str() != unlock_state::SELECTED_DEVICE_KEY {
        return Err("PIN entry requires the selected device key phone".into());
    }
    let gate =
        unlock_state::iphone_gate().map_err(|_| "unlock_idle=refused reason=gate_unavailable")?;
    let owns = || owner.validate_current().is_ok();
    let code = admitted_input(&gate, owns, || {
        pin_input::read_once(std::io::stdin().lock()).map_err(|_| "unlock_idle=refused")
    })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "unlock_idle=refused reason=runner_unavailable")?;
    let gate = Arc::new(Mutex::new(DeferredGate::new(gate)));
    let outcome = runtime.block_on(async {
        // Input may have blocked; admission must still hold before device work.
        admit_attempt(&*gate.lock().map_err(|_| "unlock_idle=refused")?, owns)?;
        let usb = PinnedUsb::new(binding.usb_id.clone());
        let runner = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120), {
            let owner = Arc::clone(&owner);
            move || owner.validate_current().is_ok()
        })
        .await
        .map_err(|_| "unlock_idle=refused reason=runner_unavailable")?;
        let liveness = runner.liveness();
        let wda = UsbWda::new(usb);
        let admit = || {
            let gate = gate.lock().map_err(|_| ())?;
            admit_attempt(&*gate, || !liveness.is_finished() && owns()).map_err(|_| ())
        };
        let action = |session: String| {
            let worker_gate = Arc::clone(&gate);
            let worker_owner = Arc::clone(&owner);
            let worker_liveness = liveness.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut gate = worker_gate.lock().map_err(|_| WorkerError::GateAccess)?;
                    let owns = move || {
                        !worker_liveness.is_finished() && worker_owner.validate_current().is_ok()
                    };
                    admit_attempt(&*gate, &owns).map_err(|_| WorkerError::Initialization)?;
                    let mut backend = NativePasscodeBackend::new_for_session(
                        binding.usb_id.clone(),
                        supervised,
                        &session,
                    )
                    .and_then(|backend| backend.with_input_admission(owns))
                    .map_err(|_| WorkerError::Initialization)?;
                    if coordinate {
                        backend = backend
                            .with_coordinate_pin_taps()
                            .map_err(|_| WorkerError::Initialization)?;
                    }
                    // Only ownership is checked at input dispatch. unlock_once
                    // deliberately marks the durable gate before digit input.
                    unlock_once(
                        &mut backend,
                        &mut *gate,
                        &binding,
                        &binding.key,
                        &code,
                        UnlockPolicy::default(),
                    )
                    .map_err(|error| {
                        WorkerError::TracedUnlock(error, backend.phase(), backend.capture_step())
                    })
                })
                .await
                .map_err(|_| WorkerError::Join)?
            }
        };
        // From runner startup onward, await the worker and restoration, then
        // close once and join even when the transaction fails.
        let transaction = with_owned_session_idle_settings_receipt(&wda, admit, action).await;
        let completed = complete(
            transaction,
            |session| async move { close_owned_session(&wda, &session).await },
            || async move {
                let alive = !runner.is_finished();
                let joined = runner.abort_and_join().await.is_ok();
                (alive, joined)
            },
        )
        .await;
        checked_outcome(completed, owns())
    })?;
    let mut gate = gate
        .lock()
        .map_err(|_| idle::diagnostic(&outcome, "gate_access"))?;
    let result = idle::finish(&mut *gate, outcome)?;
    println!("unlock_idle={result}");
    Ok(())
}

fn admit_attempt(gate: &impl AttemptGate, owns: impl FnOnce() -> bool) -> Result<(), &'static str> {
    if !owns() {
        return Err("unlock_idle=refused reason=service_ownership");
    }
    if gate.is_disabled().map_err(|_| "unlock_idle=refused")? {
        return Err("unlock_idle=disabled");
    }
    Ok(())
}

fn admitted_input<T>(
    gate: &impl AttemptGate,
    owns: impl FnOnce() -> bool,
    read: impl FnOnce() -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    admit_attempt(gate, owns)?;
    read()
}

pub(super) struct Completion<T> {
    pub(super) outcome: Result<T, OwnedSessionError>,
    pub(super) close: Option<SessionClose>,
    pub(super) runner_alive: bool,
    pub(super) runner_joined: bool,
}

pub(super) async fn complete<T, C, S>(
    transaction: Result<(String, T), OwnedSessionError>,
    close: impl FnOnce(String) -> C,
    stop: impl FnOnce() -> S,
) -> Completion<T>
where
    C: Future<Output = SessionClose>,
    S: Future<Output = (bool, bool)>,
{
    let (outcome, close) = match transaction {
        Ok((session, outcome)) => (Ok(outcome), Some(close(session).await)),
        Err(error) => (Err(error), None),
    };
    let (runner_alive, runner_joined) = stop().await;
    Completion {
        outcome,
        close,
        runner_alive,
        runner_joined,
    }
}

fn checked_outcome(completed: Completion<Outcome>, owns: bool) -> Result<Outcome, String> {
    let cleanup = completed.runner_alive
        && completed.runner_joined
        && owns
        && completed
            .close
            .is_none_or(|close| close == SessionClose::Acknowledged);
    let outcome = match completed.outcome {
        Ok(outcome) => outcome,
        Err(error) if cleanup => return Err(format!("unlock_idle={}", error.message())),
        Err(_) => return Err("unlock_idle=unknown reason=owned_cleanup_unproven".into()),
    };
    if !cleanup || completed.close != Some(SessionClose::Acknowledged) {
        return Err(idle::diagnostic(&outcome, "owned_cleanup_unproven"));
    }
    if let Some(error) = idle::failure(&outcome) {
        return Err(error);
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "owned/tests.rs"]
mod tests;
