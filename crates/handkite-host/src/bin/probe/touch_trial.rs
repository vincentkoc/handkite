use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{Action, HardwareId, LockState, Point};
use handkite_host::pin_field::valid_id as valid_session;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_action::{CoordinateTapError, WdaActions};
use handkite_host::wda_adapter::{WdaAdapter, springboard_home_launch_deadline};
use handkite_host::wda_http::HttpResponse;
use handkite_host::wda_runner::{OwnedWdaRunner, RunnerError};
use handkite_host::wda_session::{
    OwnedSessionError, SessionClose, close_owned_session, with_owned_session_idle_settings_receipt,
};
use handkite_host::wda_settings::{
    IdleAction, IdleSettingsOutcome, Restoration, with_temporary_idle_settings,
};
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, install_config, native_lock};
use quick_xml::events::Event;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLOCK: &str = "com.apple.mobiletimer";
const MAX_CAPTURE_MS: u64 = 10_000;
const MAX_AGE_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefusalReason {
    NativeLocked,
    NativeLockUnproven,
    WdaLocked,
    WdaLockUnproven,
    ForegroundNotClock,
    SessionUnproven,
    IdentityUnproven,
    FreshnessUnproven,
    CaptureFailed,
    WorldClockAttributes,
    StopwatchAttributes,
    GeometryMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrialError {
    Refused(RefusalReason),
    Unknown,
    CoordinateUnknown(CoordinateUnknown),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinateUnknown {
    Http404,
    ReceiptUnproven,
    PostconditionUnproven,
}

impl TrialError {
    fn message(self) -> &'static str {
        match self {
            Self::Refused(RefusalReason::NativeLocked) => "refused reason=native_locked",
            Self::Refused(RefusalReason::NativeLockUnproven) => {
                "refused reason=native_lock_unproven"
            }
            Self::Refused(RefusalReason::WdaLocked) => "refused reason=wda_locked",
            Self::Refused(RefusalReason::WdaLockUnproven) => "refused reason=wda_lock_unproven",
            Self::Refused(RefusalReason::ForegroundNotClock) => {
                "refused reason=foreground_not_clock"
            }
            Self::Refused(RefusalReason::SessionUnproven) => "refused reason=session_unproven",
            Self::Refused(RefusalReason::IdentityUnproven) => "refused reason=identity_unproven",
            Self::Refused(RefusalReason::FreshnessUnproven) => "refused reason=freshness_unproven",
            Self::Refused(RefusalReason::CaptureFailed) => "refused reason=capture_failed",
            Self::Refused(RefusalReason::WorldClockAttributes) => {
                "refused reason=world_clock_attributes"
            }
            Self::Refused(RefusalReason::StopwatchAttributes) => {
                "refused reason=stopwatch_attributes"
            }
            Self::Refused(RefusalReason::GeometryMismatch) => "refused reason=geometry_mismatch",
            Self::Unknown => "unknown",
            Self::CoordinateUnknown(CoordinateUnknown::Http404) => "unknown reason=http_404",
            Self::CoordinateUnknown(CoordinateUnknown::ReceiptUnproven) => {
                "unknown reason=receipt_unproven"
            }
            Self::CoordinateUnknown(CoordinateUnknown::PostconditionUnproven) => {
                "unknown reason=postcondition_unproven"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Transport {
    W3c,
    Coordinate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialTab {
    WorldClock,
    Stopwatch,
}

impl Transport {
    fn unknown(self, reason: CoordinateUnknown) -> TrialError {
        match self {
            Self::W3c => TrialError::Unknown,
            Self::Coordinate => TrialError::CoordinateUnknown(reason),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::W3c => "w3c",
            Self::Coordinate => "coordinate",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReceiptFailure {
    Trial(TrialError),
    Session(OwnedSessionError),
    Runner(RunnerError),
    LaunchUnproven,
    SettingsUnproven,
    RestorationUnproven,
    SessionCloseRefused,
    SessionCloseUnproven,
    RunnerExited,
    RunnerJoinUnproven,
    ServiceOwnership,
    TerminalError,
}

impl ReceiptFailure {
    fn name(self) -> &'static str {
        let message = match self {
            Self::Trial(error) => error.message(),
            Self::Session(error) => error.message(),
            Self::Runner(error) => {
                return match error {
                    RunnerError::InvalidBundleId => "runner_invalid_bundle_id",
                    RunnerError::InvalidTimeout => "runner_invalid_timeout",
                    RunnerError::LookupFailed => "runner_lookup_failed",
                    RunnerError::LookupTimedOut => "runner_lookup_timed_out",
                    RunnerError::PairingRequired => "runner_pairing_required",
                    RunnerError::AuthorizationRequired => "runner_authorization_required",
                    RunnerError::EndpointBusy => "runner_endpoint_busy",
                    RunnerError::EndpointUnproven => "runner_endpoint_unproven",
                    RunnerError::EndpointCleanupUnproven => "runner_endpoint_cleanup_unproven",
                    RunnerError::StartupTimedOut => "runner_startup_timed_out",
                    RunnerError::PlanStartUnproven => "runner_plan_start_unproven",
                    RunnerError::AutomationWaitTimedOut => "runner_automation_wait_timed_out",
                    RunnerError::AutomationInitializationFailed => {
                        "runner_automation_initialization_failed"
                    }
                    RunnerError::InitializationFailed => "runner_initialization_failed",
                    RunnerError::BootstrapFailed => "runner_bootstrap_failed",
                    RunnerError::RunnerExited => "runner_exited",
                    RunnerError::RunnerFailed => "runner_failed",
                    RunnerError::TaskFailed => "runner_task_failed",
                    RunnerError::OwnershipLost => "runner_ownership_lost",
                    RunnerError::ProcessIdentityUnproven => "runner_process_identity_unproven",
                    RunnerError::RunnerStopUnproven => "runner_stop_unproven",
                    RunnerError::ProfileExpired => "runner_profile_expired",
                    RunnerError::ProfileUnavailable => "runner_profile_unavailable",
                    RunnerError::NativeProcess(step) => step.as_str(),
                };
            }
            Self::LaunchUnproven => return "launch_unproven",
            Self::SettingsUnproven => return "settings_unproven",
            Self::RestorationUnproven => return "restoration_unproven",
            Self::SessionCloseRefused => return "session_close_refused",
            Self::SessionCloseUnproven => return "session_close_unproven",
            Self::RunnerExited => return "runner_exited",
            Self::RunnerJoinUnproven => return "runner_join_unproven",
            Self::ServiceOwnership => return "service_ownership",
            Self::TerminalError => return "terminal_error",
        };
        message
            .split_once("reason=")
            .map_or("unproven", |(_, reason)| reason)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StepOutcome {
    #[default]
    Skipped,
    Complete,
    Failed(ReceiptFailure),
    ObservedUnvalidated,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default)]
struct StepReceipt {
    elapsed: Option<Duration>,
    outcome: StepOutcome,
}

impl StepReceipt {
    fn finish(&mut self, started: Instant, outcome: StepOutcome) {
        self.elapsed = Some(started.elapsed());
        self.outcome = outcome;
    }

    fn measure<T>(
        &mut self,
        work: impl FnOnce() -> Result<T, TrialError>,
    ) -> Result<T, TrialError> {
        let started = Instant::now();
        let result = work();
        self.finish(
            started,
            match &result {
                Ok(_) => StepOutcome::Complete,
                Err(error) => StepOutcome::Failed(ReceiptFailure::Trial(*error)),
            },
        );
        result
    }
}

#[derive(Default)]
struct ClockReceipts {
    clock_launch: StepReceipt,
    tab_detection: StepReceipt,
    preflight_1: StepReceipt,
    preflight_2: StepReceipt,
    tap: StepReceipt,
    postcheck: StepReceipt,
}

impl ClockReceipts {
    fn unavailable() -> Self {
        let lost = StepReceipt {
            elapsed: None,
            outcome: StepOutcome::Unavailable,
        };
        Self {
            clock_launch: lost,
            tab_detection: lost,
            preflight_1: lost,
            preflight_2: lost,
            tap: lost,
            postcheck: lost,
        }
    }
}

#[derive(Default)]
struct OwnedReceipts {
    runner_startup: StepReceipt,
    session_settings_transaction: StepReceipt,
    clock: ClockReceipts,
    session_close: StepReceipt,
    runner_join: StepReceipt,
    total: StepReceipt,
}

impl OwnedReceipts {
    fn render(&self) -> String {
        let mut output = String::new();
        for (step, receipt) in [
            ("runner_startup", &self.runner_startup),
            (
                "session_settings_transaction",
                &self.session_settings_transaction,
            ),
            ("clock_launch", &self.clock.clock_launch),
            ("tab_detection", &self.clock.tab_detection),
            ("preflight_1", &self.clock.preflight_1),
            ("preflight_2", &self.clock.preflight_2),
            ("tap", &self.clock.tap),
            ("postcheck", &self.clock.postcheck),
            ("session_close", &self.session_close),
            ("runner_join", &self.runner_join),
            ("total", &self.total),
        ] {
            let (outcome, reason) = match receipt.outcome {
                StepOutcome::Skipped => ("skipped", "none"),
                StepOutcome::Complete => ("complete", "none"),
                StepOutcome::Failed(reason) => ("failed", reason.name()),
                StepOutcome::ObservedUnvalidated => ("observed_unvalidated", "receipt_unproven"),
                StepOutcome::Unavailable => ("unavailable", "worker_unproven"),
            };
            let elapsed = receipt
                .elapsed
                .map_or_else(|| "none".into(), |elapsed| elapsed.as_micros().to_string());
            writeln!(output, "touch_trial_owned_step={step} elapsed_us={elapsed} outcome={outcome} reason={reason}").expect("string formatting");
        }
        output
    }
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let (expected, transport) = options(args)?;
    let mut live =
        Live::new(expected.clone()).map_err(|reason| TrialError::Refused(reason).message())?;
    run(&mut live, &expected, transport).map_err(TrialError::message)?;
    println!("touch_trial=confirmed transport={}", transport.name());
    Ok(())
}

fn options(args: &[String]) -> Result<(HardwareId, Transport), &'static str> {
    let (config, transport) = option_syntax(args)?;
    let binding = host_config::load(config).map_err(|_| "configuration_invalid")?;
    Ok((binding.usb_id, transport))
}

fn option_syntax(args: &[String]) -> Result<(&Path, Transport), &'static str> {
    let [config_flag, config, transport_flag, transport] = args else {
        return Err("invalid_arguments");
    };
    if config_flag != "--config" || transport_flag != "--transport" {
        return Err("invalid_arguments");
    }
    let transport = match transport.as_str() {
        "w3c" => Transport::W3c,
        "coordinate" => Transport::Coordinate,
        _ => return Err("invalid_arguments"),
    };
    Ok((Path::new(config), transport))
}

pub(super) fn idle_command(args: &[String]) -> Result<(), &'static str> {
    settings_command(args)
}

pub(super) fn owned_command(args: &[String]) -> Result<(), &'static str> {
    owned_command_mode(args, false)
}

pub(super) fn owned_auto_clock_command(args: &[String]) -> Result<(), &'static str> {
    owned_command_mode(args, true)
}

fn owned_command_mode(args: &[String], auto_clock: bool) -> Result<(), &'static str> {
    let started = Instant::now();
    let mut receipts = OwnedReceipts::default();
    let result = owned_trial(args, &mut receipts, auto_clock);
    receipts.total.finish(
        started,
        match result {
            Ok(_) => StepOutcome::Complete,
            Err(_) => StepOutcome::Failed(ReceiptFailure::TerminalError),
        },
    );
    // No formatting or output while the trial or its cleanup is in progress.
    eprint!("{}", receipts.render());
    let transport = result?;
    let label = if auto_clock {
        "touch_trial_owned_auto_clock"
    } else {
        "touch_trial_owned_session"
    };
    println!("{label}=confirmed transport={}", transport.name());
    Ok(())
}

fn owned_trial(
    args: &[String],
    receipts: &mut OwnedReceipts,
    auto_clock: bool,
) -> Result<Transport, &'static str> {
    let (path, transport) = option_syntax(args)?;
    let layout = ServiceLayout::current_account().map_err(|_| "refused reason=service_layout")?;
    if path != layout.config_file() {
        return Err("refused reason=protected_config_required");
    }
    let owner =
        Arc::new(ServiceLock::acquire(layout).map_err(|_| "refused reason=service_ownership")?);
    let config = install_config::load_protected_config(owner.layout())
        .map_err(|_| "refused reason=protected_config_unavailable")?;
    let runner_id = config
        .wda_runner_bundle_id()
        .ok_or("refused reason=runner_unconfigured")?;
    let expected = config.binding().usb_id.clone();
    owner
        .validate_current()
        .map_err(|_| "refused reason=service_ownership")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "refused reason=runner_unavailable")?;
    let result = runtime.block_on(async {
        let usb = PinnedUsb::new(expected.clone());
        let started = Instant::now();
        let runner = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120), {
            let owner = Arc::clone(&owner);
            move || owner.validate_current().is_ok()
        })
        .await;
        receipts.runner_startup.finish(
            started,
            match &runner {
                Ok(_) => StepOutcome::Complete,
                Err(error) => StepOutcome::Failed(ReceiptFailure::Runner(*error)),
            },
        );
        let runner = runner.map_err(|_| "refused reason=runner_unavailable")?;
        let liveness = runner.liveness();
        let wda = UsbWda::new(usb);
        let admit = || {
            if liveness.is_finished() || owner.validate_current().is_err() {
                Err(())
            } else {
                Ok(())
            }
        };
        let clock_receipts = &mut receipts.clock;
        let action = |worker_session: String| {
            let worker_owner = Arc::clone(&owner);
            let worker_liveness = liveness.clone();
            async move {
                if worker_liveness.is_finished() || worker_owner.validate_current().is_err() {
                    return Err(TrialError::Refused(RefusalReason::SessionUnproven));
                }
                let worker = blocking_trial(move || {
                    let mut clock = ClockReceipts::default();
                    let result = (|| {
                        let input_owner = Arc::clone(&worker_owner);
                        let input_liveness = worker_liveness.clone();
                        let input_admission: Arc<dyn Fn() -> bool + Send + Sync> =
                            Arc::new(move || {
                                !input_liveness.is_finished()
                                    && input_owner.validate_current().is_ok()
                            });
                        let mut live =
                            Live::new_with_admission(expected.clone(), Some(input_admission))
                                .map_err(TrialError::Refused)?;
                        let owns = || {
                            !worker_liveness.is_finished()
                                && worker_owner.validate_current().is_ok()
                        };
                        let mut bound = SessionBound {
                            device: &mut live,
                            session: &worker_session,
                            admission: Some(&owns),
                        };
                        if auto_clock {
                            run_auto_recorded(&mut bound, &expected, transport, &mut clock)
                        } else {
                            run_recorded(&mut bound, &expected, transport, &mut clock)
                        }
                    })();
                    Ok((result, clock))
                })
                .await;
                match worker {
                    Ok((result, clock)) => {
                        *clock_receipts = clock;
                        result
                    }
                    Err(error) => {
                        // A lost worker cannot establish which steps it attempted.
                        *clock_receipts = ClockReceipts::unavailable();
                        Err(error)
                    }
                }
            }
        };
        let started = Instant::now();
        let transaction = with_owned_session_idle_settings_receipt(&wda, admit, action).await;
        receipts.session_settings_transaction.finish(
            started,
            match &transaction {
                Ok((_, outcome)) => transaction_receipt(outcome),
                Err(error) => StepOutcome::Failed(ReceiptFailure::Session(*error)),
            },
        );
        let (trial, close) = match transaction {
            Ok((session, outcome)) => {
                let trial = idle_result(outcome);
                let started = Instant::now();
                let close = close_owned_session(&wda, &session).await;
                receipts.session_close.finish(
                    started,
                    match close {
                        SessionClose::Acknowledged => StepOutcome::Complete,
                        SessionClose::Refused => {
                            StepOutcome::Failed(ReceiptFailure::SessionCloseRefused)
                        }
                        SessionClose::Unknown => {
                            StepOutcome::Failed(ReceiptFailure::SessionCloseUnproven)
                        }
                    },
                );
                (trial, Some(close))
            }
            Err(error) => (Err(error.message()), None),
        };
        let alive = !runner.is_finished();
        let started = Instant::now();
        let stopped = runner.abort_and_join().await;
        let service_owned = owner.validate_current().is_ok();
        receipts.runner_join.finish(
            started,
            if stopped.is_err() {
                StepOutcome::Failed(ReceiptFailure::RunnerJoinUnproven)
            } else if !alive {
                StepOutcome::Failed(ReceiptFailure::RunnerExited)
            } else if !service_owned {
                StepOutcome::Failed(ReceiptFailure::ServiceOwnership)
            } else {
                StepOutcome::Complete
            },
        );
        owned_result(trial, close, alive, stopped.is_ok(), service_owned)
    });
    result?;
    Ok(transport)
}

fn transaction_receipt(outcome: &IdleSettingsOutcome<(), TrialError>) -> StepOutcome {
    match (&outcome.action, outcome.restoration) {
        (IdleAction::Completed(Ok(())), Restoration::Verified) => StepOutcome::Complete,
        (IdleAction::Completed(Err(error)), Restoration::Verified) => {
            StepOutcome::Failed(ReceiptFailure::Trial(*error))
        }
        (IdleAction::NotRun(_), Restoration::NotNeeded | Restoration::Verified) => {
            StepOutcome::Failed(ReceiptFailure::SettingsUnproven)
        }
        _ => StepOutcome::Failed(ReceiptFailure::RestorationUnproven),
    }
}

fn owned_result(
    trial: Result<(), &'static str>,
    close: Option<SessionClose>,
    runner_alive: bool,
    runner_joined: bool,
    service_owned: bool,
) -> Result<(), &'static str> {
    if !runner_alive
        || !runner_joined
        || !service_owned
        || close.is_some_and(|close| close != SessionClose::Acknowledged)
    {
        Err("unknown")
    } else {
        trial
    }
}

fn settings_command(args: &[String]) -> Result<(), &'static str> {
    let (expected, transport) = options(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "refused reason=settings_unproven")?;
    runtime.block_on(async {
        let usb = PinnedUsb::new(expected.clone());
        usb.provider()
            .await
            .map_err(|_| "refused reason=identity_unproven")?;
        let wda = UsbWda::new(usb);
        let action = |worker_session: String| async move {
            blocking_trial(move || {
                // Both runtimes are constructed and dropped on the blocking thread.
                let mut live = Live::new(expected.clone()).map_err(TrialError::Refused)?;
                let mut bound = SessionBound {
                    device: &mut live,
                    session: &worker_session,
                    admission: None,
                };
                run(&mut bound, &expected, transport)
            })
            .await
        };
        let response = wda
            .request("GET", "/status", b"", Duration::from_secs(10))
            .await
            .map_err(|_| "refused reason=session_unproven")?;
        let session =
            session_from_response(response).map_err(|_| "refused reason=session_unproven")?;
        let outcome =
            with_temporary_idle_settings(&wda, &session, || action(session.clone())).await;
        idle_result(outcome)
    })?;
    println!("touch_trial_idle=confirmed transport={}", transport.name());
    Ok(())
}

async fn blocking_trial<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, TrialError> + Send + 'static,
) -> Result<T, TrialError> {
    // Always join before the transaction restores settings; never detach or retry.
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| TrialError::Unknown)?
}

fn idle_result(outcome: IdleSettingsOutcome<(), TrialError>) -> Result<(), &'static str> {
    match (outcome.action, outcome.restoration) {
        (IdleAction::Completed(result), Restoration::Verified) => {
            result.map_err(TrialError::message)
        }
        (IdleAction::NotRun(_), Restoration::NotNeeded | Restoration::Verified) => {
            Err("refused reason=settings_unproven")
        }
        _ => Err("unknown"),
    }
}

struct Snapshot {
    observation: RawObservation,
    session: String,
    native: LockState,
    home_launch_until: Option<Instant>,
    fresh_until: Instant,
}

trait TrialDevice {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason>;
    fn now_ms(&self) -> Result<u64, ()>;
    fn launch_clock(&mut self, session: &str, fresh_until: Instant) -> Result<(), TrialError>;
    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError>;
}

struct SessionBound<'a, D> {
    device: &'a mut D,
    session: &'a str,
    admission: Option<&'a dyn Fn() -> bool>,
}

impl<D: TrialDevice> TrialDevice for SessionBound<'_, D> {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
        if self.admission.is_some_and(|admit| !admit()) {
            return Err(RefusalReason::SessionUnproven);
        }
        let snapshot = self.device.capture()?;
        if snapshot.session != self.session {
            return Err(RefusalReason::SessionUnproven);
        }
        Ok(snapshot)
    }

    fn now_ms(&self) -> Result<u64, ()> {
        self.device.now_ms()
    }

    fn launch_clock(&mut self, session: &str, fresh_until: Instant) -> Result<(), TrialError> {
        if session != self.session || self.admission.is_some_and(|admit| !admit()) {
            return Err(TrialError::Refused(RefusalReason::SessionUnproven));
        }
        let result = self.device.launch_clock(session, fresh_until);
        if self.admission.is_some_and(|admit| !admit()) {
            return Err(TrialError::Unknown);
        }
        result
    }

    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError> {
        if self.admission.is_some_and(|admit| !admit()) {
            return Err(CoordinateTapError::ReceiptUnproven);
        }
        self.device.tap(transport, at, fresh_until)
    }
}

fn run(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
) -> Result<(), TrialError> {
    run_recorded(device, expected, transport, &mut ClockReceipts::default())
}

fn run_recorded(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
    receipts: &mut ClockReceipts,
) -> Result<(), TrialError> {
    run_recorded_from(
        device,
        expected,
        transport,
        InitialTab::WorldClock,
        receipts,
    )
}

fn run_auto_recorded(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
    receipts: &mut ClockReceipts,
) -> Result<(), TrialError> {
    let started = Instant::now();
    let session = device
        .capture()
        .map_err(TrialError::Refused)
        .and_then(|snapshot| {
            if snapshot.observation.connected_usb != *expected
                || snapshot.observation.wda_target != *expected
            {
                return Err(TrialError::Refused(RefusalReason::IdentityUnproven));
            }
            require_native_unlocked(snapshot.native).map_err(TrialError::Refused)?;
            if Instant::now() >= snapshot.fresh_until
                || snapshot
                    .observation
                    .window
                    .duration_ms()
                    .map_err(|_| TrialError::Refused(RefusalReason::FreshnessUnproven))?
                    > MAX_CAPTURE_MS
                || device
                    .now_ms()
                    .map_err(|_| TrialError::Refused(RefusalReason::FreshnessUnproven))?
                    .checked_sub(snapshot.observation.window.finished_unix_ms)
                    .filter(|age| *age <= MAX_AGE_MS)
                    .is_none()
            {
                return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
            }
            match snapshot.observation.state.lock {
                LockState::Unlocked => {}
                LockState::Locked => return Err(TrialError::Refused(RefusalReason::WdaLocked)),
                LockState::Uncertain
                    if snapshot
                        .home_launch_until
                        .is_some_and(|until| Instant::now() < until) => {}
                LockState::Uncertain => {
                    return Err(TrialError::Refused(RefusalReason::WdaLockUnproven));
                }
            }
            if !snapshot.observation.state.wda_ready {
                return Err(TrialError::Refused(RefusalReason::SessionUnproven));
            }
            Ok((
                snapshot.session,
                snapshot
                    .home_launch_until
                    .map_or(snapshot.fresh_until, |until| {
                        snapshot.fresh_until.min(until)
                    }),
            ))
        });
    let launch =
        session.and_then(|(session, fresh_until)| device.launch_clock(&session, fresh_until));
    receipts.clock_launch.finish(
        started,
        match launch {
            Ok(()) => StepOutcome::Complete,
            Err(TrialError::Unknown | TrialError::CoordinateUnknown(_)) => {
                StepOutcome::Failed(ReceiptFailure::LaunchUnproven)
            }
            Err(error) => StepOutcome::Failed(ReceiptFailure::Trial(error)),
        },
    );
    launch?;
    let started = Instant::now();
    let detected = device
        .capture()
        .map_err(TrialError::Refused)
        .and_then(|snapshot| {
            let world = validate_from(
                &snapshot,
                expected,
                device.now_ms(),
                false,
                InitialTab::WorldClock,
            );
            if world.is_ok() {
                return Ok(InitialTab::WorldClock);
            }
            validate_from(
                &snapshot,
                expected,
                device.now_ms(),
                false,
                InitialTab::Stopwatch,
            )
            .map(|_| InitialTab::Stopwatch)
            .map_err(|_| TrialError::Refused(world.unwrap_err()))
        });
    receipts.tab_detection.finish(
        started,
        match detected {
            Ok(_) => StepOutcome::Complete,
            Err(error) => StepOutcome::Failed(ReceiptFailure::Trial(error)),
        },
    );
    let initial = detected?;
    run_recorded_from(device, expected, transport, initial, receipts)
}

fn run_recorded_from(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
    initial: InitialTab,
    receipts: &mut ClockReceipts,
) -> Result<(), TrialError> {
    let (before, first) = receipts.preflight_1.measure(|| {
        let before = device.capture().map_err(TrialError::Refused)?;
        let first = validate_from(&before, expected, device.now_ms(), false, initial)
            .map_err(TrialError::Refused)?;
        Ok((before, first))
    })?;
    // Reobserve every precondition; only stable accessibility geometry authorizes input.
    let (current, second, input_started) = receipts.preflight_2.measure(|| {
        let current = device.capture().map_err(TrialError::Refused)?;
        let second = validate_from(&current, expected, device.now_ms(), false, initial)
            .map_err(TrialError::Refused)?;
        if current.session != before.session {
            return Err(TrialError::Refused(RefusalReason::SessionUnproven));
        }
        if current.observation.window.started_unix_ms < before.observation.window.finished_unix_ms {
            return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
        }
        if first != second {
            return Err(TrialError::Refused(RefusalReason::GeometryMismatch));
        }
        let input_started = device
            .now_ms()
            .map_err(|_| TrialError::Refused(RefusalReason::FreshnessUnproven))?;
        Ok((current, second, input_started))
    })?;
    let started = Instant::now();
    let receipt = device.tap(transport, second.center(), current.fresh_until);
    let receipt_error = receipt.err().map(|error| match error {
        CoordinateTapError::Http404 => CoordinateUnknown::Http404,
        CoordinateTapError::ReceiptUnproven => CoordinateUnknown::ReceiptUnproven,
    });
    receipts.tap.finish(
        started,
        receipt_error.map_or(StepOutcome::Complete, |reason| {
            StepOutcome::Failed(ReceiptFailure::Trial(TrialError::CoordinateUnknown(reason)))
        }),
    );
    // Even unknown delivery receives one read-only postcheck, never another input.
    let started = Instant::now();
    let after = device.capture();
    // Keep the receipt category even when the sole postcapture also fails.
    if let Some(reason) = receipt_error {
        receipts.postcheck.finish(
            started,
            match after {
                Ok(_) => StepOutcome::ObservedUnvalidated,
                Err(reason) => {
                    StepOutcome::Failed(ReceiptFailure::Trial(TrialError::Refused(reason)))
                }
            },
        );
        return Err(transport.unknown(reason));
    }
    let postcondition = transport.unknown(CoordinateUnknown::PostconditionUnproven);
    let result = (|| {
        let after = after.map_err(TrialError::Refused)?;
        validate_from(&after, expected, device.now_ms(), true, initial)
            .map_err(TrialError::Refused)?;
        if after.session != current.session {
            return Err(TrialError::Refused(RefusalReason::SessionUnproven));
        }
        if after.observation.window.started_unix_ms < input_started {
            return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
        }
        Ok(())
    })();
    receipts.postcheck.finish(
        started,
        match result {
            Ok(()) => StepOutcome::Complete,
            Err(error) => StepOutcome::Failed(ReceiptFailure::Trial(error)),
        },
    );
    result.map_err(|_| postcondition)
}

fn validate_from(
    snapshot: &Snapshot,
    expected: &HardwareId,
    now: Result<u64, ()>,
    after: bool,
    initial: InitialTab,
) -> Result<Rect, RefusalReason> {
    let raw = &snapshot.observation;
    if Instant::now() >= snapshot.fresh_until {
        return Err(RefusalReason::FreshnessUnproven);
    }
    if raw.connected_usb != *expected || raw.wda_target != *expected {
        return Err(RefusalReason::IdentityUnproven);
    }
    if !valid_session(&snapshot.session) {
        return Err(RefusalReason::SessionUnproven);
    }
    require_native_unlocked(snapshot.native)?;
    match raw.state.lock {
        LockState::Locked => return Err(RefusalReason::WdaLocked),
        LockState::Uncertain => return Err(RefusalReason::WdaLockUnproven),
        LockState::Unlocked => {}
    }
    if !raw.state.wda_ready {
        return Err(RefusalReason::SessionUnproven);
    }
    if raw.state.foreground_app.as_deref() != Some(CLOCK) {
        return Err(RefusalReason::ForegroundNotClock);
    }
    if raw
        .window
        .duration_ms()
        .map_err(|_| RefusalReason::FreshnessUnproven)?
        > MAX_CAPTURE_MS
        || now
            .map_err(|_| RefusalReason::FreshnessUnproven)?
            .checked_sub(raw.window.finished_unix_ms)
            .ok_or(RefusalReason::FreshnessUnproven)?
            > MAX_AGE_MS
    {
        return Err(RefusalReason::FreshnessUnproven);
    }
    let png = &raw.screenshot.bytes;
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || &png[12..16] != b"IHDR" {
        return Err(RefusalReason::CaptureFailed);
    }
    let width = u32::from_be_bytes(
        png[16..20]
            .try_into()
            .map_err(|_| RefusalReason::CaptureFailed)?,
    );
    let height = u32::from_be_bytes(
        png[20..24]
            .try_into()
            .map_err(|_| RefusalReason::CaptureFailed)?,
    );
    let evidence = parse(&raw.accessibility_tree)?;
    // Coordinates come from accessibility points; image pixels only check screen shape.
    if width == 0
        || height == 0
        || (f64::from(width) / f64::from(height) - evidence.screen.w / evidence.screen.h).abs()
            > 0.02
    {
        return Err(RefusalReason::GeometryMismatch);
    }
    let stopwatch_active = (initial == InitialTab::Stopwatch) != after;
    let explicit_selected = evidence.stopwatch.selected_present || evidence.world.selected_present;
    if !evidence
        .stopwatch
        .selection_matches(stopwatch_active, explicit_selected)
    {
        return Err(RefusalReason::StopwatchAttributes);
    }
    if !evidence
        .world
        .selection_matches(!stopwatch_active, explicit_selected)
    {
        return Err(RefusalReason::WorldClockAttributes);
    }
    if evidence.world_clock_navigation != u8::from(!stopwatch_active) {
        return Err(RefusalReason::WorldClockAttributes);
    }
    let start_matches = if stopwatch_active {
        evidence.start_ready
    } else {
        evidence.start_hidden_or_absent
    };
    if !start_matches {
        return Err(RefusalReason::StopwatchAttributes);
    }
    Ok(if initial == InitialTab::WorldClock {
        evidence.stopwatch.rect
    } else {
        evidence.world.rect
    })
}

fn require_native_unlocked(state: LockState) -> Result<(), RefusalReason> {
    match state {
        LockState::Locked => Err(RefusalReason::NativeLocked),
        LockState::Uncertain => Err(RefusalReason::NativeLockUnproven),
        LockState::Unlocked => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

type Attributes<'a> = BTreeMap<&'a str, Cow<'a, str>>;

impl Rect {
    fn read(attrs: &Attributes<'_>) -> Result<Self, ()> {
        let number = |name| attrs.get(name).ok_or(())?.parse::<f64>().map_err(|_| ());
        let rect = Self {
            x: number("x")?,
            y: number("y")?,
            w: number("width")?,
            h: number("height")?,
        };
        if [rect.x, rect.y, rect.w, rect.h]
            .iter()
            .any(|n| !n.is_finite() || !(0.0..=10_000.0).contains(n))
            || rect.w <= 0.0
            || rect.h <= 0.0
        {
            return Err(());
        }
        Ok(rect)
    }

    fn contains(self, other: Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.w <= self.x + self.w
            && other.y + other.h <= self.y + self.h
    }

    fn center(self) -> Point {
        Point {
            x: self.x + self.w / 2.0,
            y: self.y + self.h / 2.0,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TabValue {
    Absent,
    Active,
    Other,
}

struct Tab {
    rect: Rect,
    visible: Option<bool>,
    enabled: Option<bool>,
    selected: Option<bool>,
    selected_present: bool,
    value: TabValue,
}

impl Tab {
    fn read(attrs: &Attributes<'_>) -> Result<Self, ()> {
        let flag = |key| match attrs.get(key).map(|value| value.as_ref()) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        Ok(Self {
            rect: Rect::read(attrs)?,
            visible: flag("visible"),
            enabled: flag("enabled"),
            selected: flag("selected"),
            selected_present: attrs.contains_key("selected"),
            value: match attrs.get("value").map(|value| value.as_ref()) {
                None => TabValue::Absent,
                Some("1") => TabValue::Active,
                _ => TabValue::Other,
            },
        })
    }

    fn eligible(&self) -> bool {
        self.visible == Some(true) && self.enabled == Some(true)
    }

    fn selection_matches(&self, active: bool, explicit_selected: bool) -> bool {
        if explicit_selected {
            self.selected == Some(active)
                && match self.value {
                    TabValue::Absent => true,
                    TabValue::Active => active,
                    TabValue::Other => false,
                }
        } else {
            !self.selected_present
                && self.value
                    == if active {
                        TabValue::Active
                    } else {
                        TabValue::Absent
                    }
        }
    }
}

struct Evidence {
    screen: Rect,
    stopwatch: Tab,
    world: Tab,
    world_clock_navigation: u8,
    start_hidden_or_absent: bool,
    start_ready: bool,
}

fn parse(xml: &str) -> Result<Evidence, RefusalReason> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(RefusalReason::CaptureFailed);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut screen = None;
    let mut tabbar = None;
    let mut stopwatch = None;
    let mut world = None;
    let mut world_clock_navigation = 0u8;
    let mut start_hidden_or_absent = true;
    let mut start_ready = false;
    let mut start_seen = false;
    loop {
        let event = reader
            .read_event()
            .map_err(|_| RefusalReason::CaptureFailed)?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                let tag = node.name();
                let kind =
                    std::str::from_utf8(tag.as_ref()).map_err(|_| RefusalReason::CaptureFailed)?;
                if stack.len() > 128 {
                    return Err(RefusalReason::CaptureFailed);
                }
                if matches!(
                    kind,
                    "XCUIElementTypeAlert"
                        | "XCUIElementTypeSheet"
                        | "XCUIElementTypeSecureTextField"
                ) {
                    return Err(RefusalReason::WdaLockUnproven);
                }
                let mut attrs = BTreeMap::new();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| RefusalReason::CaptureFailed)?;
                    let key = std::str::from_utf8(attr.key.into_inner())
                        .map_err(|_| RefusalReason::CaptureFailed)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| RefusalReason::CaptureFailed)?;
                    if [
                        "proud-lock",
                        "passcode-view",
                        "call-to-action-label",
                        "Enter Passcode",
                    ]
                    .iter()
                    .any(|marker| value.contains(marker))
                    {
                        return Err(RefusalReason::WdaLocked);
                    }
                    attrs.insert(key, value);
                }
                if kind == "XCUIElementTypeApplication" {
                    if screen.is_some() {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    let rect = Rect::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?;
                    if rect.x != 0.0 || rect.y != 0.0 {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    screen = Some(rect);
                }
                if kind == "XCUIElementTypeTabBar" {
                    if tabbar.is_some() || !stack.iter().any(|s| s == "XCUIElementTypeApplication")
                    {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    tabbar = Some(Rect::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?);
                }
                let name = attrs.get("name").map(|value| value.as_ref());
                let label = attrs.get("label").map(|value| value.as_ref());
                if kind == "XCUIElementTypeNavigationBar"
                    && (name == Some("World Clock") || label == Some("World Clock"))
                    && attrs.get("visible").map(|value| value.as_ref()) == Some("true")
                {
                    if !stack.iter().any(|s| s == "XCUIElementTypeApplication") {
                        return Err(RefusalReason::WorldClockAttributes);
                    }
                    if !stack.iter().any(|s| s == "XCUIElementTypeTabBar") {
                        world_clock_navigation = world_clock_navigation.saturating_add(1);
                    }
                }
                if matches!(name, Some("Stopwatch" | "World Clock"))
                    && kind == "XCUIElementTypeButton"
                {
                    if !stack.iter().any(|s| s == "XCUIElementTypeTabBar") {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    let (slot, reason) = if name == Some("Stopwatch") {
                        (&mut stopwatch, RefusalReason::StopwatchAttributes)
                    } else {
                        (&mut world, RefusalReason::WorldClockAttributes)
                    };
                    if slot.is_some() {
                        return Err(reason);
                    }
                    *slot = Some(Tab::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?);
                }
                if (name == Some("Start") || label == Some("Start"))
                    && kind == "XCUIElementTypeButton"
                    && !stack.iter().any(|s| s == "XCUIElementTypeTabBar")
                {
                    if start_seen || !stack.iter().any(|s| s == "XCUIElementTypeApplication") {
                        return Err(RefusalReason::StopwatchAttributes);
                    }
                    start_seen = true;
                    let button = Tab::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?;
                    start_hidden_or_absent = button.visible == Some(false);
                    start_ready = button.visible == Some(true)
                        && button.enabled == Some(true)
                        && screen
                            .ok_or(RefusalReason::GeometryMismatch)?
                            .contains(button.rect);
                }
                if is_start {
                    stack.push(kind.to_owned());
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(RefusalReason::CaptureFailed)?;
            }
            Event::Eof => break,
            Event::DocType(_) => return Err(RefusalReason::CaptureFailed),
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(RefusalReason::CaptureFailed);
    }
    let evidence = Evidence {
        screen: screen.ok_or(RefusalReason::GeometryMismatch)?,
        stopwatch: stopwatch.ok_or(RefusalReason::StopwatchAttributes)?,
        world: world.ok_or(RefusalReason::WorldClockAttributes)?,
        world_clock_navigation,
        start_hidden_or_absent,
        start_ready,
    };
    let bar = tabbar.ok_or(RefusalReason::GeometryMismatch)?;
    if !evidence.stopwatch.eligible() {
        return Err(RefusalReason::StopwatchAttributes);
    }
    if !evidence.world.eligible() {
        return Err(RefusalReason::WorldClockAttributes);
    }
    if !evidence.screen.contains(bar)
        || !bar.contains(evidence.stopwatch.rect)
        || !bar.contains(evidence.world.rect)
        || evidence.stopwatch.rect.overlaps(evidence.world.rect)
    {
        return Err(RefusalReason::GeometryMismatch);
    }
    Ok(evidence)
}

struct Live {
    expected: HardwareId,
    adapter: WdaAdapter,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
}

impl Live {
    fn new(expected: HardwareId) -> Result<Self, RefusalReason> {
        Self::new_with_admission(expected, None)
    }

    fn new_with_admission(
        expected: HardwareId,
        input_admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self, RefusalReason> {
        Ok(Self {
            adapter: WdaAdapter::new(expected.clone()).map_err(|_| RefusalReason::CaptureFailed)?,
            wda: UsbWda::new(PinnedUsb::new(expected.clone()))
                .with_write_admission(input_admission),
            expected,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| RefusalReason::CaptureFailed)?,
            actions: WdaActions::default(),
        })
    }

    fn status(&self) -> Result<String, RefusalReason> {
        let response = self
            .runtime
            .block_on(
                self.wda
                    .request("GET", "/status", b"", Duration::from_secs(10)),
            )
            .map_err(|_| RefusalReason::SessionUnproven)?;
        session_from_response(response)
    }
}

fn session_from_response(response: HttpResponse) -> Result<String, RefusalReason> {
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| RefusalReason::SessionUnproven)?;
    let session = json["sessionId"]
        .as_str()
        .filter(|id| valid_session(id))
        .ok_or(RefusalReason::SessionUnproven)?;
    if response.status != 200
        || json["value"]["ready"] != true
        || json["value"].get("error").is_some()
    {
        return Err(RefusalReason::SessionUnproven);
    }
    Ok(session.to_owned())
}

impl TrialDevice for Live {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
        let started = self
            .now_ms()
            .map_err(|_| RefusalReason::FreshnessUnproven)?;
        self.adapter
            .identities()
            .map_err(|_| RefusalReason::IdentityUnproven)?;
        let session = self.status()?;
        let native_before = native_lock::query(&self.expected).state;
        require_native_unlocked(native_before)?;
        let (mut observation, captured_at, reported_lock) = self
            .adapter
            .observe_with_attributes_and_lock_report()
            .map_err(|_| RefusalReason::CaptureFailed)?;
        let fresh_until = captured_at
            .checked_add(Duration::from_millis(MAX_AGE_MS))
            .ok_or(RefusalReason::FreshnessUnproven)?;
        self.runtime
            .block_on(self.actions.prepare_existing(&self.wda))
            .map_err(|_| RefusalReason::SessionUnproven)?;
        if !self.actions.prepared_session_matches(&session) || self.status()? != session {
            return Err(RefusalReason::SessionUnproven);
        }
        let native = native_lock::query(&self.expected).state;
        let home_launch_until = springboard_home_launch_deadline(
            &observation,
            native_before,
            native,
            reported_lock,
            captured_at,
        );
        self.adapter
            .identities()
            .map_err(|_| RefusalReason::IdentityUnproven)?;
        // Include all identity/session/native checks in the bounded capture window.
        observation.window.started_unix_ms = started;
        Ok(Snapshot {
            observation,
            session,
            native,
            home_launch_until,
            fresh_until,
        })
    }

    fn now_ms(&self) -> Result<u64, ()> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_millis() as u64)
            .map_err(|_| ())
    }

    fn launch_clock(&mut self, session: &str, fresh_until: Instant) -> Result<(), TrialError> {
        if Instant::now() >= fresh_until {
            return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
        }
        self.adapter
            .identities()
            .map_err(|_| TrialError::Refused(RefusalReason::IdentityUnproven))?;
        require_native_unlocked(native_lock::query(&self.expected).state)
            .map_err(TrialError::Refused)?;
        if self.status().map_err(TrialError::Refused)? != session {
            return Err(TrialError::Refused(RefusalReason::SessionUnproven));
        }
        self.runtime
            .block_on(self.actions.prepare_existing(&self.wda))
            .map_err(|_| TrialError::Refused(RefusalReason::SessionUnproven))?;
        if !self.actions.prepared_session_matches(session) {
            return Err(TrialError::Refused(RefusalReason::SessionUnproven));
        }
        let receipt = self
            .runtime
            .block_on(self.actions.execute_existing(
                &self.wda.with_write_deadline(fresh_until),
                &Action::LaunchApp {
                    bundle_id: CLOCK.into(),
                },
            ))
            .map_err(|_| TrialError::Unknown)?;
        if receipt.accepted != Some(true) || receipt.action_kind != "launch_app" {
            return Err(TrialError::Unknown);
        }
        if self.status().map_err(|_| TrialError::Unknown)? != session {
            return Err(TrialError::Unknown);
        }
        Ok(())
    }

    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError> {
        let wda = self.wda.with_write_deadline(fresh_until);
        let receipt = match transport {
            Transport::W3c => self
                .runtime
                .block_on(self.actions.execute_existing(&wda, &Action::Tap { at }))
                .map_err(|_| CoordinateTapError::ReceiptUnproven),
            Transport::Coordinate => self
                .runtime
                .block_on(self.actions.tap_existing_diagnostic(&wda, at)),
        }?;
        if receipt.accepted == Some(true) {
            Ok(())
        } else {
            Err(CoordinateTapError::ReceiptUnproven)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use std::collections::VecDeque;

    type RefusalMutation = (RefusalReason, fn(&mut Snapshot));

    const REFUSAL_OUTPUTS: &[(RefusalReason, &str)] = &[
        (
            RefusalReason::NativeLocked,
            "touch_trial=refused reason=native_locked",
        ),
        (
            RefusalReason::NativeLockUnproven,
            "touch_trial=refused reason=native_lock_unproven",
        ),
        (
            RefusalReason::WdaLocked,
            "touch_trial=refused reason=wda_locked",
        ),
        (
            RefusalReason::WdaLockUnproven,
            "touch_trial=refused reason=wda_lock_unproven",
        ),
        (
            RefusalReason::ForegroundNotClock,
            "touch_trial=refused reason=foreground_not_clock",
        ),
        (
            RefusalReason::SessionUnproven,
            "touch_trial=refused reason=session_unproven",
        ),
        (
            RefusalReason::IdentityUnproven,
            "touch_trial=refused reason=identity_unproven",
        ),
        (
            RefusalReason::FreshnessUnproven,
            "touch_trial=refused reason=freshness_unproven",
        ),
        (
            RefusalReason::CaptureFailed,
            "touch_trial=refused reason=capture_failed",
        ),
        (
            RefusalReason::WorldClockAttributes,
            "touch_trial=refused reason=world_clock_attributes",
        ),
        (
            RefusalReason::StopwatchAttributes,
            "touch_trial=refused reason=stopwatch_attributes",
        ),
        (
            RefusalReason::GeometryMismatch,
            "touch_trial=refused reason=geometry_mismatch",
        ),
    ];

    fn tree(after: bool) -> String {
        format!(
            r#"<AppiumAUT><XCUIElementTypeApplication name="Clock" x="0" y="0" width="400" height="800">
          <XCUIElementTypeNavigationBar name="World Clock" visible="{before}"/>
          <XCUIElementTypeButton name="Start" visible="{after}" enabled="true" x="300" y="500" width="60" height="60"/>
          <XCUIElementTypeTabBar x="0" y="700" width="400" height="100">
            <XCUIElementTypeButton name="World Clock" visible="true" enabled="true" selected="{before}" x="0" y="700" width="100" height="80"/>
            <XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="{after}" x="200" y="700" width="100" height="80"/>
          </XCUIElementTypeTabBar>
        </XCUIElementTypeApplication></AppiumAUT>"#,
            before = !after
        )
    }

    const WORLD_NAV: &str = r#"<XCUIElementTypeNavigationBar name="World Clock" visible="true"/>"#;
    const HIDDEN_START: &str = r#"<XCUIElementTypeButton name="Start" visible="false" enabled="true" x="300" y="500" width="60" height="60"/>"#;
    const READY_START: &str = r#"<XCUIElementTypeButton name="Start" visible="true" enabled="true" x="300" y="500" width="60" height="60"/>"#;

    #[test]
    fn borrowed_attributes_still_decode_and_check_unused_values() {
        assert!(parse(&tree(false).replace("World Clock", "World&#32;Clock")).is_ok());
        let marker = tree(false).replace(
            "name=\"Clock\"",
            "name=\"Clock\" ignored=\"Enter&#32;Passcode\"",
        );
        assert!(matches!(parse(&marker), Err(RefusalReason::WdaLocked)));
        let malformed =
            tree(false).replace("name=\"Clock\"", "name=\"Clock\" ignored=\"&unknown;\"");
        assert!(matches!(
            parse(&malformed),
            Err(RefusalReason::CaptureFailed)
        ));
    }

    fn value_schema(xml: &str) -> String {
        xml.replace(" selected=\"true\"", " value=\"1\"")
            .replace(" selected=\"false\"", "")
    }

    fn snapshot(after: bool, tick: u64) -> Snapshot {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        Snapshot {
            observation: RawObservation {
                connected_usb: HardwareId::new("fixture-device").unwrap(),
                wda_target: HardwareId::new("fixture-device").unwrap(),
                window: ObservationWindow {
                    started_unix_ms: tick,
                    finished_unix_ms: tick + 1,
                },
                state: DeviceState {
                    lock: LockState::Unlocked,
                    wda_ready: true,
                    foreground_app: Some(CLOCK.into()),
                },
                accessibility_tree: tree(after),
                screenshot: ScreenImage {
                    format: ImageFormat::Png,
                    bytes: png,
                },
            },
            session: "fixture-session".into(),
            native: LockState::Unlocked,
            home_launch_until: None,
            fresh_until: Instant::now() + Duration::from_millis(MAX_AGE_MS),
        }
    }

    struct Fixture {
        captures: VecDeque<Result<Snapshot, RefusalReason>>,
        launches: Vec<String>,
        launch_result: Result<(), TrialError>,
        taps: Vec<(Transport, Point, Instant)>,
        receipt: Result<(), CoordinateTapError>,
        now: u64,
        age: u64,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                captures: [
                    Ok(snapshot(false, 100)),
                    Ok(snapshot(false, 200)),
                    Ok(snapshot(true, 300)),
                ]
                .into(),
                launches: Vec::new(),
                launch_result: Ok(()),
                taps: Vec::new(),
                receipt: Ok(()),
                now: 0,
                age: 10,
            }
        }
        fn run(&mut self, transport: Transport) -> Result<(), TrialError> {
            run(self, &HardwareId::new("fixture-device").unwrap(), transport)
        }
    }

    impl TrialDevice for Fixture {
        fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
            let snapshot = self
                .captures
                .pop_front()
                .expect("unexpected capture or retry");
            if let Ok(snapshot) = &snapshot {
                self.now = snapshot.observation.window.finished_unix_ms + self.age;
            }
            snapshot
        }
        fn now_ms(&self) -> Result<u64, ()> {
            Ok(self.now)
        }
        fn launch_clock(&mut self, session: &str, fresh_until: Instant) -> Result<(), TrialError> {
            if Instant::now() >= fresh_until {
                return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
            }
            self.launches.push(session.to_owned());
            self.launch_result
        }
        fn tap(
            &mut self,
            transport: Transport,
            at: Point,
            fresh_until: Instant,
        ) -> Result<(), CoordinateTapError> {
            assert!(fresh_until > Instant::now());
            assert!(self.taps.is_empty(), "second input or transport fallback");
            self.taps.push((transport, at, fresh_until));
            self.receipt
        }
    }

    fn run_bound(
        fixture: &mut Fixture,
        transport: Transport,
        session: &str,
    ) -> Result<(), TrialError> {
        let mut bound = SessionBound {
            device: fixture,
            session,
            admission: None,
        };
        run(
            &mut bound,
            &HardwareId::new("fixture-device").unwrap(),
            transport,
        )
    }

    fn recorded(
        fixture: &mut Fixture,
        transport: Transport,
    ) -> (Result<(), TrialError>, ClockReceipts) {
        let mut receipts = ClockReceipts::default();
        let result = run_recorded(
            fixture,
            &HardwareId::new("fixture-device").unwrap(),
            transport,
            &mut receipts,
        );
        (result, receipts)
    }

    #[test]
    fn reverse_clock_transition_taps_world_clock_once_after_two_stopwatch_preflights() {
        let mut fixture = Fixture::new();
        fixture.captures = [
            Ok(snapshot(true, 100)),
            Ok(snapshot(true, 200)),
            Ok(snapshot(false, 300)),
        ]
        .into();
        let mut receipts = ClockReceipts::default();
        assert_eq!(
            run_recorded_from(
                &mut fixture,
                &HardwareId::new("fixture-device").unwrap(),
                Transport::W3c,
                InitialTab::Stopwatch,
                &mut receipts,
            ),
            Ok(())
        );
        assert_eq!(fixture.taps.len(), 1);
        assert_eq!(fixture.taps[0].1, Point { x: 50.0, y: 740.0 });
        assert!(fixture.captures.is_empty());
        assert_eq!(receipts.tap.outcome, StepOutcome::Complete);
        assert_eq!(receipts.postcheck.outcome, StepOutcome::Complete);
    }

    #[test]
    fn auto_clock_launches_once_then_proves_either_tab_transition() {
        for initial_stopwatch in [false, true] {
            let mut fixture = Fixture::new();
            let mut before_launch = snapshot(false, 50);
            before_launch.observation.state.foreground_app = Some("com.example.other".into());
            fixture.captures = [
                Ok(before_launch),
                Ok(snapshot(initial_stopwatch, 100)),
                Ok(snapshot(initial_stopwatch, 200)),
                Ok(snapshot(initial_stopwatch, 300)),
                Ok(snapshot(!initial_stopwatch, 400)),
            ]
            .into();
            let mut receipts = ClockReceipts::default();
            assert_eq!(
                run_auto_recorded(
                    &mut fixture,
                    &HardwareId::new("fixture-device").unwrap(),
                    Transport::W3c,
                    &mut receipts,
                ),
                Ok(())
            );
            assert_eq!(fixture.launches, ["fixture-session"]);
            assert_eq!(fixture.taps.len(), 1);
            assert_eq!(
                fixture.taps[0].1,
                if initial_stopwatch {
                    Point { x: 50.0, y: 740.0 }
                } else {
                    Point { x: 250.0, y: 740.0 }
                }
            );
            assert_eq!(receipts.clock_launch.outcome, StepOutcome::Complete);
            assert_eq!(receipts.tab_detection.outcome, StepOutcome::Complete);
            assert_eq!(receipts.tap.outcome, StepOutcome::Complete);
            assert_eq!(receipts.postcheck.outcome, StepOutcome::Complete);
        }
    }

    #[test]
    fn springboard_home_launch_requires_native_and_wda_agreement() {
        let mut home = snapshot(false, 50);
        home.observation.state.foreground_app = Some("com.apple.springboard".into());
        home.observation.state.lock = LockState::Uncertain;
        let home_tree = "<XCUIElementTypeApplication name=\"SpringBoard\"><XCUIElementTypeIcon name=\"Clock\"/></XCUIElementTypeApplication>";
        home.observation.accessibility_tree = home_tree.into();
        let captured_at = Instant::now();
        assert!(
            springboard_home_launch_deadline(
                &home.observation,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false),
                captured_at,
            )
            .is_some()
        );
        for (native, reported) in [
            (LockState::Locked, Some(false)),
            (LockState::Uncertain, Some(false)),
            (LockState::Unlocked, Some(true)),
            (LockState::Unlocked, None),
        ] {
            assert!(
                springboard_home_launch_deadline(
                    &home.observation,
                    native,
                    LockState::Unlocked,
                    reported,
                    captured_at,
                )
                .is_none()
            );
        }
        for native_after in [LockState::Locked, LockState::Uncertain] {
            assert!(
                springboard_home_launch_deadline(
                    &home.observation,
                    LockState::Unlocked,
                    native_after,
                    Some(false),
                    captured_at,
                )
                .is_none()
            );
        }
        for marker in [
            "proud-lock",
            "passcode-view",
            "call-to-action-label",
            "Enter Passcode",
            "XCUIElementTypeAlert",
            "XCUIElementTypeSheet",
            "XCUIElementTypeSecureTextField",
        ] {
            let mut blocked = home.observation.accessibility_tree.clone();
            blocked.push_str(marker);
            home.observation.accessibility_tree = blocked;
            assert!(
                springboard_home_launch_deadline(
                    &home.observation,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false),
                    captured_at,
                )
                .is_none()
            );
            home.observation.accessibility_tree = home_tree.into();
        }
        home.observation.accessibility_tree =
            "<XCUIElementTypeApplication name=\"SpringBoard\"/>".into();
        assert!(
            springboard_home_launch_deadline(
                &home.observation,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false),
                captured_at,
            )
            .is_none()
        );
        home.observation.accessibility_tree = home_tree.into();
        home.home_launch_until = springboard_home_launch_deadline(
            &home.observation,
            LockState::Unlocked,
            LockState::Unlocked,
            Some(false),
            captured_at,
        );
        let mut fixture = Fixture::new();
        fixture.captures = [
            Ok(home),
            Ok(snapshot(false, 100)),
            Ok(snapshot(false, 200)),
            Ok(snapshot(false, 300)),
            Ok(snapshot(true, 400)),
        ]
        .into();
        let mut receipts = ClockReceipts::default();
        assert_eq!(
            run_auto_recorded(
                &mut fixture,
                &HardwareId::new("fixture-device").unwrap(),
                Transport::W3c,
                &mut receipts,
            ),
            Ok(())
        );
        assert_eq!(fixture.launches, ["fixture-session"]);
        assert_eq!(fixture.taps.len(), 1);

        let mut stale = snapshot(false, 50);
        stale.observation.state.foreground_app = Some("com.apple.springboard".into());
        stale.observation.state.lock = LockState::Uncertain;
        stale.home_launch_until = Some(Instant::now() - Duration::from_millis(1));
        stale.fresh_until = Instant::now() - Duration::from_millis(1);
        let mut fixture = Fixture::new();
        fixture.captures = [Ok(stale)].into();
        let mut receipts = ClockReceipts::default();
        assert_eq!(
            run_auto_recorded(
                &mut fixture,
                &HardwareId::new("fixture-device").unwrap(),
                Transport::W3c,
                &mut receipts,
            ),
            Err(TrialError::Refused(RefusalReason::FreshnessUnproven))
        );
        assert!(fixture.launches.is_empty());
        assert!(fixture.taps.is_empty());
    }

    #[test]
    fn auto_clock_unknown_launch_or_lost_admission_never_taps() {
        let expected = HardwareId::new("fixture-device").unwrap();
        let mut fixture = Fixture::new();
        fixture.captures = [Ok(snapshot(false, 50))].into();
        fixture.launch_result = Err(TrialError::Unknown);
        let mut receipts = ClockReceipts::default();
        assert_eq!(
            run_auto_recorded(&mut fixture, &expected, Transport::W3c, &mut receipts),
            Err(TrialError::Unknown)
        );
        assert_eq!(fixture.launches, ["fixture-session"]);
        assert!(fixture.taps.is_empty());
        assert_eq!(
            receipts.clock_launch.outcome,
            StepOutcome::Failed(ReceiptFailure::LaunchUnproven)
        );
        assert_eq!(receipts.tab_detection.outcome, StepOutcome::Skipped);

        let mut fixture = Fixture::new();
        fixture.captures = [Ok(snapshot(false, 50))].into();
        let calls = std::cell::Cell::new(0);
        let admit = || {
            calls.set(calls.get() + 1);
            calls.get() < 3
        };
        let mut bound = SessionBound {
            device: &mut fixture,
            session: "fixture-session",
            admission: Some(&admit),
        };
        let mut receipts = ClockReceipts::default();
        assert_eq!(
            run_auto_recorded(&mut bound, &expected, Transport::W3c, &mut receipts),
            Err(TrialError::Unknown)
        );
        assert_eq!(fixture.launches, ["fixture-session"]);
        assert!(fixture.taps.is_empty());
        assert_eq!(
            receipts.clock_launch.outcome,
            StepOutcome::Failed(ReceiptFailure::LaunchUnproven)
        );
    }

    #[test]
    fn auto_clock_refuses_locked_or_changed_session_before_launch() {
        let expected = HardwareId::new("fixture-device").unwrap();
        for changed_session in [false, true] {
            let mut fixture = Fixture::new();
            let mut initial = snapshot(false, 50);
            if changed_session {
                initial.session = "other-session".into();
            } else {
                initial.native = LockState::Locked;
            }
            fixture.captures = [Ok(initial)].into();
            let mut bound = SessionBound {
                device: &mut fixture,
                session: "fixture-session",
                admission: None,
            };
            let mut receipts = ClockReceipts::default();
            let result = run_auto_recorded(&mut bound, &expected, Transport::W3c, &mut receipts);
            assert_eq!(
                result,
                Err(TrialError::Refused(if changed_session {
                    RefusalReason::SessionUnproven
                } else {
                    RefusalReason::NativeLocked
                }))
            );
            assert!(fixture.launches.is_empty());
            assert!(fixture.taps.is_empty());
        }
        for (lock, ready, expected_reason) in [
            (LockState::Locked, true, RefusalReason::WdaLocked),
            (LockState::Uncertain, true, RefusalReason::WdaLockUnproven),
            (LockState::Unlocked, false, RefusalReason::SessionUnproven),
        ] {
            let mut fixture = Fixture::new();
            let mut initial = snapshot(false, 50);
            initial.observation.state.lock = lock;
            initial.observation.state.wda_ready = ready;
            fixture.captures = [Ok(initial)].into();
            let mut receipts = ClockReceipts::default();
            assert_eq!(
                run_auto_recorded(&mut fixture, &expected, Transport::W3c, &mut receipts),
                Err(TrialError::Refused(expected_reason))
            );
            assert!(fixture.launches.is_empty());
            assert!(fixture.taps.is_empty());
        }
    }

    #[test]
    fn auto_clock_unexpected_tab_refuses_after_launch_without_tap() {
        let expected = HardwareId::new("fixture-device").unwrap();
        let mut fixture = Fixture::new();
        let mut unsupported = snapshot(false, 100);
        unsupported.observation.accessibility_tree = tree(false)
            .replace("selected=\"true\"", "selected=\"false\"")
            .replace(
                "name=\"World Clock\" visible=\"true\"",
                "name=\"World Clock\" visible=\"false\"",
            );
        fixture.captures = [Ok(snapshot(false, 50)), Ok(unsupported)].into();
        let mut receipts = ClockReceipts::default();
        assert!(run_auto_recorded(&mut fixture, &expected, Transport::W3c, &mut receipts).is_err());
        assert_eq!(fixture.launches, ["fixture-session"]);
        assert!(fixture.taps.is_empty());
        assert!(matches!(
            receipts.tab_detection.outcome,
            StepOutcome::Failed(_)
        ));
    }

    #[test]
    fn recorded_trial_keeps_capture_count_and_marks_only_attempted_steps() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for failure in [None, Some(0), Some(1)] {
                let mut fixture = Fixture::new();
                if let Some(index) = failure {
                    fixture.captures[index] = Err(RefusalReason::CaptureFailed);
                }
                let (result, receipts) = recorded(&mut fixture, transport);
                assert_eq!(result.is_ok(), failure.is_none());
                let steps = [
                    receipts.preflight_1,
                    receipts.preflight_2,
                    receipts.tap,
                    receipts.postcheck,
                ];
                for (index, step) in steps.iter().enumerate() {
                    if failure == Some(index) {
                        assert_eq!(
                            step.outcome,
                            StepOutcome::Failed(ReceiptFailure::Trial(TrialError::Refused(
                                RefusalReason::CaptureFailed
                            )))
                        );
                        assert!(step.elapsed.is_some());
                    } else if failure.is_some_and(|failure| index > failure) {
                        assert_eq!(step.outcome, StepOutcome::Skipped);
                        assert!(step.elapsed.is_none());
                    } else {
                        assert_eq!(step.outcome, StepOutcome::Complete);
                        assert!(step.elapsed.is_some());
                    }
                }
                assert_eq!(fixture.taps.len(), usize::from(failure.is_none()));
                assert_eq!(fixture.captures.len(), failure.map_or(0, |index| 2 - index));
            }
        }
    }

    #[test]
    fn recorded_receipt_error_does_not_validate_or_hide_the_sole_postcapture() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for error in [
                CoordinateTapError::Http404,
                CoordinateTapError::ReceiptUnproven,
            ] {
                for capture_fails in [false, true] {
                    let mut fixture = Fixture::new();
                    fixture.receipt = Err(error);
                    if capture_fails {
                        fixture.captures[2] = Err(RefusalReason::CaptureFailed);
                    } else {
                        // Deliberately invalid evidence must remain unvalidated after receipt loss.
                        fixture.captures[2].as_mut().unwrap().native = LockState::Locked;
                    }
                    let (result, receipts) = recorded(&mut fixture, transport);
                    let reason = match error {
                        CoordinateTapError::Http404 => CoordinateUnknown::Http404,
                        CoordinateTapError::ReceiptUnproven => CoordinateUnknown::ReceiptUnproven,
                    };
                    assert_eq!(result, Err(transport.unknown(reason)));
                    assert_eq!(
                        receipts.tap.outcome,
                        StepOutcome::Failed(ReceiptFailure::Trial(TrialError::CoordinateUnknown(
                            reason
                        )))
                    );
                    assert_eq!(
                        receipts.postcheck.outcome,
                        if capture_fails {
                            StepOutcome::Failed(ReceiptFailure::Trial(TrialError::Refused(
                                RefusalReason::CaptureFailed,
                            )))
                        } else {
                            StepOutcome::ObservedUnvalidated
                        }
                    );
                    assert!(receipts.postcheck.elapsed.is_some());
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn recorded_postcondition_failure_preserves_terminal_category() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            let mut fixture = Fixture::new();
            fixture.captures[2].as_mut().unwrap().native = LockState::Locked;
            let (result, receipts) = recorded(&mut fixture, transport);
            assert_eq!(
                result,
                Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
            );
            assert_eq!(
                receipts.postcheck.outcome,
                StepOutcome::Failed(ReceiptFailure::Trial(TrialError::Refused(
                    RefusalReason::NativeLocked
                )))
            );
            assert_eq!(fixture.taps.len(), 1);
            assert!(fixture.captures.is_empty());
        }
    }

    #[test]
    fn aggregate_receipt_preserves_settings_restoration_precedence() {
        use handkite_host::wda_settings::SettingsError;
        let failed = Err(TrialError::CoordinateUnknown(CoordinateUnknown::Http404));
        for action in [
            IdleAction::Completed(Ok(())),
            IdleAction::Completed(failed),
            IdleAction::NotRun(SettingsError::ReadUnproven),
        ] {
            let outcome = IdleSettingsOutcome {
                action,
                restoration: Restoration::Unproven(SettingsError::WriteUnproven),
            };
            assert_eq!(
                transaction_receipt(&outcome),
                StepOutcome::Failed(ReceiptFailure::RestorationUnproven)
            );
            assert_eq!(idle_result(outcome), Err("unknown"));
        }
        let outcome = IdleSettingsOutcome {
            action: IdleAction::Completed(failed),
            restoration: Restoration::Verified,
        };
        assert_eq!(
            transaction_receipt(&outcome),
            StepOutcome::Failed(ReceiptFailure::Trial(TrialError::CoordinateUnknown(
                CoordinateUnknown::Http404
            )))
        );
        assert_eq!(idle_result(outcome), Err("unknown reason=http_404"));
    }

    #[test]
    fn receipt_output_has_only_fixed_fields_and_distinguishes_lost_worker_from_skipped() {
        let mut fixture = Fixture::new();
        fixture.captures[0].as_mut().unwrap().session = "private-session-value".into();
        fixture.captures[0]
            .as_mut()
            .unwrap()
            .observation
            .connected_usb = HardwareId::new("private-device-value").unwrap();
        let (_, clock) = recorded(&mut fixture, Transport::W3c);
        let mut report = OwnedReceipts {
            clock,
            ..OwnedReceipts::default()
        };
        report.clock.preflight_1.elapsed = Some(Duration::from_micros(123));
        let output = report.render();
        assert_eq!(output.lines().count(), 11);
        assert!(output.contains("touch_trial_owned_step=preflight_1 elapsed_us=123 outcome=failed reason=identity_unproven\n"));
        assert!(
            output.contains(
                "touch_trial_owned_step=tap elapsed_us=none outcome=skipped reason=none\n"
            )
        );
        assert!(!output.contains("private-"));
        assert!(!output.contains("fixture-"));
        for line in output.lines() {
            assert_eq!(line.split_ascii_whitespace().count(), 4);
            assert!(line.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b"_= ".contains(&byte)));
        }
        report.clock = ClockReceipts::unavailable();
        let output = report.render();
        assert_eq!(
            output
                .matches("outcome=unavailable reason=worker_unproven")
                .count(),
            6
        );
        assert!(output.contains(
            "touch_trial_owned_step=session_close elapsed_us=none outcome=skipped reason=none\n"
        ));
    }

    #[test]
    fn runner_startup_receipt_preserves_only_fixed_failure_categories() {
        let failures = [
            (RunnerError::InvalidBundleId, "runner_invalid_bundle_id"),
            (RunnerError::InvalidTimeout, "runner_invalid_timeout"),
            (RunnerError::LookupFailed, "runner_lookup_failed"),
            (RunnerError::LookupTimedOut, "runner_lookup_timed_out"),
            (RunnerError::PairingRequired, "runner_pairing_required"),
            (
                RunnerError::AuthorizationRequired,
                "runner_authorization_required",
            ),
            (RunnerError::EndpointBusy, "runner_endpoint_busy"),
            (RunnerError::EndpointUnproven, "runner_endpoint_unproven"),
            (
                RunnerError::EndpointCleanupUnproven,
                "runner_endpoint_cleanup_unproven",
            ),
            (RunnerError::StartupTimedOut, "runner_startup_timed_out"),
            (RunnerError::PlanStartUnproven, "runner_plan_start_unproven"),
            (
                RunnerError::AutomationWaitTimedOut,
                "runner_automation_wait_timed_out",
            ),
            (
                RunnerError::AutomationInitializationFailed,
                "runner_automation_initialization_failed",
            ),
            (
                RunnerError::InitializationFailed,
                "runner_initialization_failed",
            ),
            (RunnerError::BootstrapFailed, "runner_bootstrap_failed"),
            (RunnerError::RunnerExited, "runner_exited"),
            (RunnerError::RunnerFailed, "runner_failed"),
            (RunnerError::TaskFailed, "runner_task_failed"),
            (RunnerError::OwnershipLost, "runner_ownership_lost"),
            (
                RunnerError::ProcessIdentityUnproven,
                "runner_process_identity_unproven",
            ),
            (RunnerError::RunnerStopUnproven, "runner_stop_unproven"),
            (RunnerError::ProfileExpired, "runner_profile_expired"),
            (
                RunnerError::ProfileUnavailable,
                "runner_profile_unavailable",
            ),
            (
                RunnerError::NativeProcess(
                    handkite_host::wda_runner::NativeProcessStep::InventoryPath,
                ),
                "process_inventory_path",
            ),
        ];
        for (error, reason) in failures {
            let report = OwnedReceipts {
                runner_startup: StepReceipt {
                    elapsed: Some(Duration::from_micros(1)),
                    outcome: StepOutcome::Failed(ReceiptFailure::Runner(error)),
                },
                ..OwnedReceipts::default()
            };
            let output = report.render();
            assert!(output.contains(&format!(
                "touch_trial_owned_step=runner_startup elapsed_us=1 outcome=failed reason={reason}\n"
            )));
            assert_eq!(output.lines().count(), 11);
        }
    }

    #[test]
    fn idle_trial_binds_both_pre_captures_to_the_settings_session() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            // Both Clock captures agree with each other but not the transaction.
            let mut wrong = Fixture::new();
            assert_eq!(
                run_bound(&mut wrong, transport, "different-valid-session"),
                Err(TrialError::Refused(RefusalReason::SessionUnproven))
            );
            assert!(wrong.taps.is_empty());
            assert_eq!(wrong.captures.len(), 2);

            let mut drift = Fixture::new();
            drift.captures[1].as_mut().unwrap().session = "replacement-session".into();
            assert_eq!(
                run_bound(&mut drift, transport, "fixture-session"),
                Err(TrialError::Refused(RefusalReason::SessionUnproven))
            );
            assert!(drift.taps.is_empty());
            assert_eq!(drift.captures.len(), 1);
        }
    }

    #[test]
    fn idle_trial_post_session_mismatch_is_unknown_without_replay() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for changed in [false, true] {
                let mut fixture = Fixture::new();
                if changed {
                    fixture.captures[2].as_mut().unwrap().session = "replacement-session".into();
                }
                assert_eq!(
                    run_bound(&mut fixture, transport, "fixture-session"),
                    if changed {
                        Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                    } else {
                        Ok(())
                    }
                );
                assert_eq!(fixture.taps.len(), 1);
                assert!(fixture.captures.is_empty());
            }
        }
    }

    #[test]
    fn owned_trial_refuses_lost_admission_before_input() {
        use std::cell::Cell;
        for fail_on in [1, 2, 3] {
            let checks = Cell::new(0);
            let admit = || {
                checks.set(checks.get() + 1);
                checks.get() < fail_on
            };
            let mut fixture = Fixture::new();
            let mut bound = SessionBound {
                device: &mut fixture,
                session: "fixture-session",
                admission: Some(&admit),
            };
            assert!(
                run(
                    &mut bound,
                    &HardwareId::new("fixture-device").unwrap(),
                    Transport::W3c
                )
                .is_err()
            );
            assert!(fixture.taps.is_empty());
        }
    }

    #[test]
    fn runner_exit_after_clock_captures_prevents_tap() {
        use std::cell::Cell;
        let runner_alive = Cell::new(true);
        let checks = Cell::new(0);
        let admit = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                // Both captures are allowed; the runner exits before the tap guard.
                runner_alive.set(false);
                true
            } else {
                runner_alive.get()
            }
        };
        let mut fixture = Fixture::new();
        let mut bound = SessionBound {
            device: &mut fixture,
            session: "fixture-session",
            admission: Some(&admit),
        };
        assert_eq!(
            run(
                &mut bound,
                &HardwareId::new("fixture-device").unwrap(),
                Transport::W3c,
            ),
            Err(TrialError::Unknown)
        );
        assert_eq!(checks.get(), 4);
        assert!(fixture.taps.is_empty());
        assert_eq!(fixture.captures.len(), 1);
    }

    #[test]
    fn owned_trial_never_confirms_uncertain_cleanup() {
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), true, true, true),
            Ok(())
        );
        for close in [SessionClose::Refused, SessionClose::Unknown] {
            assert_eq!(
                owned_result(Ok(()), Some(close), true, true, true),
                Err("unknown")
            );
        }
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), false, true, true),
            Err("unknown")
        );
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), true, false, true),
            Err("unknown")
        );
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), true, true, false),
            Err("unknown")
        );
        assert_eq!(
            owned_result(
                Err("refused reason=session_admission"),
                None,
                true,
                true,
                true
            ),
            Err("refused reason=session_admission")
        );
    }

    #[test]
    fn idle_bridge_requires_verified_restore_and_preserves_fixed_outcomes() {
        use handkite_host::wda_settings::SettingsError;
        for restoration in [
            Restoration::NotNeeded,
            Restoration::Verified,
            Restoration::Unproven(SettingsError::WriteUnproven),
        ] {
            for result in [
                Ok(()),
                Err(TrialError::Unknown),
                Err(TrialError::CoordinateUnknown(CoordinateUnknown::Http404)),
                Err(TrialError::CoordinateUnknown(
                    CoordinateUnknown::ReceiptUnproven,
                )),
                Err(TrialError::CoordinateUnknown(
                    CoordinateUnknown::PostconditionUnproven,
                )),
                Err(TrialError::Refused(RefusalReason::SessionUnproven)),
            ] {
                let expected = if restoration == Restoration::Verified {
                    result.map_err(TrialError::message)
                } else {
                    Err("unknown")
                };
                assert_eq!(
                    idle_result(IdleSettingsOutcome {
                        action: IdleAction::Completed(result),
                        restoration,
                    }),
                    expected
                );
            }
            assert_eq!(
                idle_result(IdleSettingsOutcome {
                    action: IdleAction::NotRun(SettingsError::SessionUnproven),
                    restoration,
                }),
                if matches!(restoration, Restoration::Unproven(_)) {
                    Err("unknown")
                } else {
                    Err("refused reason=settings_unproven")
                }
            );
        }
    }

    #[tokio::test]
    async fn blocking_bridge_awaits_worker_runtime_and_maps_join_panic_to_unknown() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        assert_eq!(
            blocking_trial(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    worker_calls.fetch_add(1, Ordering::SeqCst);
                });
                Ok(())
            })
            .await,
            Ok(())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            blocking_trial::<()>(|| panic!("fixture worker panic")).await,
            Err(TrialError::Unknown)
        );
    }

    #[test]
    fn each_transport_sends_once_at_accessibility_center_then_observes_once() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            let mut fixture = Fixture::new();
            let expires = fixture.captures[1].as_ref().unwrap().fresh_until;
            assert_eq!(fixture.run(transport), Ok(()));
            assert_eq!(
                fixture.taps,
                [(transport, Point { x: 250.0, y: 740.0 }, expires)]
            );
            assert!(fixture.captures.is_empty());
        }
    }

    #[test]
    fn both_coherent_schemas_require_the_reciprocal_content_transition() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for schema in 0..3 {
                for start_absent in [false, true] {
                    let mut fixture = Fixture::new();
                    for (index, capture) in fixture.captures.iter_mut().enumerate() {
                        let xml = &mut capture.as_mut().unwrap().observation.accessibility_tree;
                        if schema == 1 {
                            *xml = value_schema(xml);
                        }
                        if schema == 2 {
                            *xml =
                                xml.replace("selected=\"true\"", "selected=\"true\" value=\"1\"");
                        }
                        if start_absent && index < 2 {
                            *xml = xml.replace(HIDDEN_START, "");
                        }
                        *xml = xml
                            .replace(
                                "NavigationBar name=\"World Clock\"",
                                "NavigationBar label=\"World Clock\"",
                            )
                            .replace("Button name=\"Start\"", "Button label=\"Start\"");
                    }
                    assert_eq!(fixture.run(transport), Ok(()));
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn mixed_conflicting_or_unrecognized_selection_evidence_refuses_both_transports() {
        let explicit = tree(false);
        let values = value_schema(&explicit);
        let inactive = "name=\"Stopwatch\" visible=\"true\" enabled=\"true\"";
        let cases = [
            explicit.replace("selected=\"true\"", "selected=\"false\""),
            explicit.replace("selected=\"false\"", "selected=\"true\""),
            explicit.replace("selected=\"false\"", "selected=\"invalid\""),
            explicit.replace(" selected=\"false\"", ""),
            explicit.replace(" selected=\"true\"", ""),
            explicit.replace("selected=\"false\"", "selected=\"false\" value=\"1\""),
            explicit.replace("selected=\"true\"", "selected=\"true\" value=\"other\""),
            values.replace(" value=\"1\"", ""),
            values.replace(inactive, &format!("{inactive} value=\"1\"")),
            values.replace(inactive, &format!("{inactive} value=\"0\"")),
            values.replace(inactive, &format!("{inactive} selected=\"false\"")),
            values.replace("value=\"1\"", "selected=\"true\" value=\"1\""),
            values.replace("value=\"1\"", "selected=\"invalid\" value=\"1\""),
            values.replace("value=\"1\"", "value=\"0\""),
            values.replace("value=\"1\"", "value=\"true\""),
            values.replace("value=\"1\"", "value=\"false\""),
            values.replace("value=\"1\"", "value=\" 1\""),
            values.replace("value=\"1\"", "value=\"\""),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for position in 0..2 {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    fixture.captures[position]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = xml.clone();
                    assert!(matches!(
                        fixture.run(transport),
                        Err(TrialError::Refused(_))
                    ));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn before_content_rejects_other_clock_tabs_overlays_and_ambiguous_markers() {
        let before = tree(false);
        let cases = [
            before.replace(WORLD_NAV, &WORLD_NAV.replace("World Clock", "Alarm")),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("World Clock", "Timers")),
            before.replace(WORLD_NAV, ""),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("true", "false")),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("NavigationBar", "StaticText")),
            before.replace(WORLD_NAV, &WORLD_NAV.repeat(2)),
            before
                .replace(WORLD_NAV, "")
                .replace("</AppiumAUT>", &format!("{WORLD_NAV}</AppiumAUT>")),
            before.replace(WORLD_NAV, "").replace(
                "</XCUIElementTypeTabBar>",
                &format!("{WORLD_NAV}</XCUIElementTypeTabBar>"),
            ),
            before.replace(HIDDEN_START, READY_START),
            before.replace(
                HIDDEN_START,
                &READY_START.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            before.replace(
                HIDDEN_START,
                &HIDDEN_START.replace(" visible=\"false\"", ""),
            ),
            before.replace(HIDDEN_START, &HIDDEN_START.repeat(2)),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
            ),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeSheet/></XCUIElementTypeApplication>",
            ),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeSecureTextField/></XCUIElementTypeApplication>",
            ),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for values in [false, true] {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    fixture.captures[1]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = if values {
                        value_schema(xml)
                    } else {
                        xml.clone()
                    };
                    assert!(matches!(
                        fixture.run(transport),
                        Err(TrialError::Refused(_))
                    ));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn invalid_after_content_or_schema_is_unknown_after_exactly_one_tap() {
        let after = tree(true);
        let cases = [
            after.replace(&WORLD_NAV.replace("true", "false"), WORLD_NAV),
            after.replace(READY_START, ""),
            after.replace(READY_START, HIDDEN_START),
            after.replace(
                READY_START,
                &READY_START.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            after.replace(READY_START, &READY_START.repeat(2)),
            after
                .replace(READY_START, "")
                .replace("</AppiumAUT>", &format!("{READY_START}</AppiumAUT>")),
            after.replace(READY_START, "").replace(
                "</XCUIElementTypeTabBar>",
                &format!("{READY_START}</XCUIElementTypeTabBar>"),
            ),
            after.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
            ),
            after.replace("selected=\"false\"", "selected=\"true\""),
            after.replace(" selected=\"true\"", ""),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for values in [false, true] {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    if values {
                        for capture in &mut fixture.captures {
                            let source =
                                &mut capture.as_mut().unwrap().observation.accessibility_tree;
                            *source = value_schema(source);
                        }
                    }
                    fixture.captures[2]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = if values {
                        value_schema(xml)
                    } else {
                        xml.clone()
                    };
                    assert_eq!(
                        fixture.run(transport),
                        Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                    );
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn both_preflight_captures_reject_stale_mismatched_locked_and_unready_evidence() {
        use RefusalReason::*;
        let mutations: &[RefusalMutation] = &[
            (FreshnessUnproven, |s| s.fresh_until = Instant::now()),
            (IdentityUnproven, |s| {
                s.observation.connected_usb = HardwareId::new("other-device").unwrap()
            }),
            (IdentityUnproven, |s| {
                s.observation.wda_target = HardwareId::new("other-device").unwrap()
            }),
            (NativeLocked, |s| s.native = LockState::Locked),
            (NativeLockUnproven, |s| s.native = LockState::Uncertain),
            (WdaLocked, |s| s.observation.state.lock = LockState::Locked),
            (WdaLockUnproven, |s| {
                s.observation.state.lock = LockState::Uncertain
            }),
            (SessionUnproven, |s| s.observation.state.wda_ready = false),
            (ForegroundNotClock, |s| {
                s.observation.state.foreground_app = Some("com.apple.springboard".into())
            }),
            (SessionUnproven, |s| s.session = "../invalid".into()),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 0
            }),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 20_000
            }),
            (CaptureFailed, |s| s.observation.screenshot.bytes.clear()),
            (StopwatchAttributes, |s| {
                s.observation.accessibility_tree = tree(true)
            }),
            (WorldClockAttributes, |s| {
                s.observation.accessibility_tree =
                    tree(false).replace("selected=\"true\"", "selected=\"false\"")
            }),
            (GeometryMismatch, |s| {
                s.observation.accessibility_tree = tree(false).replace("x=\"200\"", "x=\"NaN\"")
            }),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for position in 0..2 {
                for (reason, mutate) in mutations {
                    let mut fixture = Fixture::new();
                    mutate(fixture.captures[position].as_mut().unwrap());
                    assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
        let mut fixture = Fixture::new();
        fixture.age = 4000;
        assert_eq!(
            fixture.run(Transport::Coordinate),
            Err(TrialError::Refused(FreshnessUnproven))
        );
        assert!(fixture.taps.is_empty());
    }

    #[test]
    fn revalidation_refuses_changed_session_geometry_or_reused_capture() {
        let mutations: &[RefusalMutation] = &[
            (RefusalReason::SessionUnproven, |s| {
                s.session = "replacement-session".into()
            }),
            (RefusalReason::GeometryMismatch, |s| {
                s.observation.accessibility_tree = tree(false).replace("x=\"200\"", "x=\"210\"")
            }),
            (RefusalReason::FreshnessUnproven, |s| {
                s.observation.window.started_unix_ms = 100
            }),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (reason, mutate) in mutations {
                let mut fixture = Fixture::new();
                mutate(fixture.captures[1].as_mut().unwrap());
                assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                assert!(fixture.taps.is_empty());
            }
        }
    }

    #[test]
    fn ambiguous_hidden_disabled_missing_attributes_and_unsafe_trees_refuse_input() {
        let original = tree(false);
        use RefusalReason::*;
        let duplicate = r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="false" x="200" y="700" width="100" height="80"/>"#;
        let cases = [
            (
                StopwatchAttributes,
                original.replace(
                    "</XCUIElementTypeTabBar>",
                    &format!("{duplicate}</XCUIElementTypeTabBar>"),
                ),
            ),
            (
                StopwatchAttributes,
                original.replace("visible=\"true\"", "visible=\"false\""),
            ),
            (
                StopwatchAttributes,
                original.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            (
                StopwatchAttributes,
                original.replace(" selected=\"false\"", ""),
            ),
            (
                StopwatchAttributes,
                original.replace(" visible=\"true\"", ""),
            ),
            (
                StopwatchAttributes,
                original.replace(" enabled=\"true\"", ""),
            ),
            (
                WorldClockAttributes,
                original.replace("name=\"World Clock\"", "name=\"Other\""),
            ),
            (GeometryMismatch, original.replace("x=\"200\"", "x=\"NaN\"")),
            (GeometryMismatch, original.replace("x=\"200\"", "x=\"399\"")),
            (
                WdaLockUnproven,
                original.replace("</AppiumAUT>", "<XCUIElementTypeAlert/></AppiumAUT>"),
            ),
            (
                WdaLockUnproven,
                original.replace(
                    "</AppiumAUT>",
                    "<XCUIElementTypeSecureTextField/></AppiumAUT>",
                ),
            ),
            (
                WdaLocked,
                original.replace("</AppiumAUT>", "<node name=\"proud-lock\"/></AppiumAUT>"),
            ),
            (CaptureFailed, original.replace("</AppiumAUT>", "")),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (reason, xml) in &cases {
                let mut fixture = Fixture::new();
                fixture.captures[1]
                    .as_mut()
                    .unwrap()
                    .observation
                    .accessibility_tree = xml.clone();
                assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                assert!(fixture.taps.is_empty());
            }
        }
    }

    #[test]
    fn acknowledged_input_without_the_postcondition_remains_unknown_without_retry() {
        let mutations: &[fn(&mut Snapshot)] = &[
            |s| s.observation.accessibility_tree = tree(false),
            |s| {
                s.observation.accessibility_tree =
                    tree(true).replace("name=\"Start\"", "name=\"Other\"")
            },
            |s| s.native = LockState::Locked,
            |s| s.session = "replacement-session".into(),
            |s| s.observation.connected_usb = HardwareId::new("other-device").unwrap(),
            |s| s.observation.window.started_unix_ms = 100,
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for mutate in mutations {
                let mut fixture = Fixture::new();
                mutate(fixture.captures[2].as_mut().unwrap());
                assert_eq!(
                    fixture.run(transport),
                    Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                );
                assert_eq!(fixture.taps.len(), 1);
                assert!(fixture.captures.is_empty());
            }
        }
        let mut fixture = Fixture::new();
        fixture.captures[2] = Err(RefusalReason::CaptureFailed);
        assert_eq!(
            fixture.run(Transport::Coordinate),
            Err(TrialError::CoordinateUnknown(
                CoordinateUnknown::PostconditionUnproven
            ))
        );
        assert_eq!(fixture.taps.len(), 1);
    }

    #[test]
    fn unknown_receipt_keeps_its_fixed_category_after_exactly_one_postcheck() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (receipt, reason, output) in [
                (
                    CoordinateTapError::Http404,
                    CoordinateUnknown::Http404,
                    "touch_trial=unknown reason=http_404",
                ),
                (
                    CoordinateTapError::ReceiptUnproven,
                    CoordinateUnknown::ReceiptUnproven,
                    "touch_trial=unknown reason=receipt_unproven",
                ),
            ] {
                for post in 0..3 {
                    let mut fixture = Fixture::new();
                    fixture.receipt = Err(receipt);
                    match post {
                        0 => {}
                        1 => fixture.captures[2] = Err(RefusalReason::CaptureFailed),
                        _ => {
                            fixture.captures[2]
                                .as_mut()
                                .unwrap()
                                .observation
                                .accessibility_tree = tree(false)
                        }
                    }
                    let error = fixture.run(transport).unwrap_err();
                    assert_eq!(error, transport.unknown(reason));
                    assert_eq!(
                        format!("touch_trial={}", error.message()),
                        if transport == Transport::Coordinate {
                            output
                        } else {
                            "touch_trial=unknown"
                        }
                    );
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn capture_failures_have_fixed_output_and_never_report_refusal_after_input() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for &(reason, output) in REFUSAL_OUTPUTS {
                for position in 0..3 {
                    let mut fixture = Fixture::new();
                    fixture.captures[position] = Err(reason);
                    let error = fixture.run(transport).unwrap_err();
                    if position < 2 {
                        assert_eq!(error, TrialError::Refused(reason));
                        assert_eq!(format!("touch_trial={}", error.message()), output);
                        assert!(fixture.taps.is_empty());
                    } else {
                        assert_eq!(
                            error,
                            transport.unknown(CoordinateUnknown::PostconditionUnproven)
                        );
                        let expected = if transport == Transport::Coordinate {
                            "touch_trial=unknown reason=postcondition_unproven"
                        } else {
                            "touch_trial=unknown"
                        };
                        assert_eq!(format!("touch_trial={}", error.message()), expected);
                        assert_eq!(fixture.taps.len(), 1);
                    }
                    assert_eq!(fixture.captures.len(), 2 - position);
                }
            }
        }
    }

    #[test]
    fn first_failed_guard_keeps_its_original_precedence() {
        use RefusalReason::*;
        let expected = HardwareId::new("fixture-device").unwrap();
        let mut sample = snapshot(false, 100);
        sample.fresh_until = Instant::now();
        sample.observation.connected_usb = HardwareId::new("other-device").unwrap();
        sample.session.clear();
        sample.native = LockState::Locked;
        sample.observation.state.lock = LockState::Locked;
        sample.observation.state.wda_ready = false;
        sample.observation.state.foreground_app = None;
        sample.observation.window.finished_unix_ms = 0;
        let repairs: &[RefusalMutation] = &[
            (FreshnessUnproven, |s| {
                s.fresh_until = Instant::now() + Duration::from_millis(MAX_AGE_MS)
            }),
            (IdentityUnproven, |s| {
                s.observation.connected_usb = HardwareId::new("fixture-device").unwrap()
            }),
            (SessionUnproven, |s| s.session = "fixture-session".into()),
            (NativeLocked, |s| s.native = LockState::Unlocked),
            (WdaLocked, |s| {
                s.observation.state.lock = LockState::Unlocked
            }),
            (SessionUnproven, |s| s.observation.state.wda_ready = true),
            (ForegroundNotClock, |s| {
                s.observation.state.foreground_app = Some(CLOCK.into())
            }),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 101
            }),
        ];
        for (reason, repair) in repairs {
            assert_eq!(
                validate_from(&sample, &expected, Err(()), false, InitialTab::WorldClock),
                Err(*reason)
            );
            repair(&mut sample);
        }
        assert_eq!(
            validate_from(&sample, &expected, Err(()), false, InitialTab::WorldClock),
            Err(FreshnessUnproven)
        );
        assert!(validate_from(&sample, &expected, Ok(1000), false, InitialTab::WorldClock).is_ok());
    }

    #[test]
    fn arguments_and_config_fail_before_device_initialization() {
        for args in [
            vec![],
            vec!["--transport", "w3c"],
            vec!["--config", "missing", "--transport", "fallback"],
            vec!["--config", "missing", "--transport", "w3c", "extra"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(command(&args), Err("invalid_arguments"));
            assert_eq!(idle_command(&args), Err("invalid_arguments"));
            assert_eq!(owned_command(&args), Err("invalid_arguments"));
            assert_eq!(owned_auto_clock_command(&args), Err("invalid_arguments"));
        }
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.json");
        std::fs::write(
            &config,
            br#"{"device_key":"fixture","usb_id":"fixture-device"}"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            idle_command(&[
                "--config".into(),
                config.to_str().unwrap().into(),
                "--transport".into(),
                "w3c".into()
            ]),
            Err("configuration_invalid")
        );
        assert_eq!(
            command(&[
                "--config".into(),
                config.to_str().unwrap().into(),
                "--transport".into(),
                "w3c".into()
            ]),
            Err("configuration_invalid")
        );
    }
}
