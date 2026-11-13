//! One owned-runner startup check. It sends no phone input or WDA session request.
//! External controller exclusion must be established before invoking this binary.

use handkite_host::install_config;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_runner::{OwnedWdaRunner, RunnerError};
use std::time::Duration;
use std::time::Instant;

#[path = "handkited/signals.rs"]
mod signals;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() != ["--service"] {
        println!("runner_check=invalid");
        std::process::exit(2);
    }
    let result = run();
    match result {
        Ok(()) => println!("runner_check=startup_ready joined=true endpoint_vacant=true"),
        Err(reason) => {
            println!("runner_check=refused reason={reason}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), &'static str> {
    let _signals = signals::ShutdownSignals::install().map_err(|_| "signal_setup")?;
    let layout = ServiceLayout::current_account().map_err(|_| "service_layout")?;
    let owner = ServiceLock::acquire(layout).map_err(|_| "service_ownership")?;
    let config =
        install_config::load_protected_config(owner.layout()).map_err(|_| "protected_config")?;
    let runner_id = config.wda_runner_bundle_id().ok_or("runner_unconfigured")?;
    let usb = PinnedUsb::new(config.binding().usb_id.clone());
    owner.validate_current().map_err(|_| "service_ownership")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "runtime_unavailable")?;
    let startup = runtime.block_on(async {
        tokio::select! {
            result = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120)) =>
                result.map_err(runner_reason),
            _ = wait_shutdown() => Err("shutdown_requested"),
        }
    });
    let result = match startup {
        Ok(runner) => {
            println!("runner_check_stage=startup_ready");
            let alive = !runner.is_finished();
            let joined = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), runner.abort_and_join())
                    .await
                    .is_ok_and(|result| result.is_ok())
            });
            if !alive {
                Err("runner_liveness_unproven")
            } else if !joined {
                Err("runner_join_unproven")
            } else {
                println!("runner_check_stage=runner_joined");
                Ok(())
            }
        }
        Err(reason) => Err(reason),
    };
    runtime.shutdown_timeout(Duration::from_secs(5));
    if signals::requested() {
        return Err("shutdown_requested");
    }
    if owner.validate_current().is_err() {
        return Err("service_ownership");
    }
    result?;
    let check = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "runtime_unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if signals::requested() {
            return Err("shutdown_requested");
        }
        owner.validate_current().map_err(|_| "service_ownership")?;
        match check.block_on(handkite_host::wda_runner::endpoint_vacant(&usb)) {
            Ok(()) => break,
            Err(RunnerError::EndpointBusy) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(RunnerError::EndpointBusy) => return Err("endpoint_cleanup_timeout"),
            Err(_) => return Err("endpoint_unproven"),
        }
    }
    Ok(())
}

async fn wait_shutdown() {
    while !signals::requested() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn runner_reason(error: RunnerError) -> &'static str {
    match error {
        RunnerError::InvalidBundleId => "invalid_runner_id",
        RunnerError::InvalidTimeout => "invalid_timeout",
        RunnerError::LookupFailed => "lookup_failed",
        RunnerError::LookupTimedOut => "lookup_timed_out",
        RunnerError::EndpointBusy => "endpoint_busy",
        RunnerError::EndpointUnproven => "endpoint_unproven",
        RunnerError::EndpointCleanupUnproven => "endpoint_cleanup_unproven",
        RunnerError::StartupTimedOut => "startup_timed_out",
        RunnerError::PlanStartUnproven => "plan_start_unproven",
        RunnerError::AutomationAuthorizationRequired => "automation_authorization_required",
        RunnerError::InitializationFailed => "initialization_failed",
        RunnerError::BootstrapFailed => "bootstrap_failed",
        RunnerError::RunnerExited => "runner_exited",
        RunnerError::RunnerFailed => "runner_failed",
        RunnerError::TaskFailed => "task_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_errors_have_fixed_private_safe_reasons() {
        for error in [
            RunnerError::EndpointBusy,
            RunnerError::EndpointCleanupUnproven,
            RunnerError::PlanStartUnproven,
            RunnerError::AutomationAuthorizationRequired,
            RunnerError::InitializationFailed,
            RunnerError::BootstrapFailed,
            RunnerError::RunnerFailed,
        ] {
            let reason = runner_reason(error);
            assert!(
                reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            );
        }
    }
}
