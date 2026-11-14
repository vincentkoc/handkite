//! Own one XCTest task on an explicitly pinned USB device.
//!
//! This library does not acquire exclusive device ownership. Callers must establish
//! that ownership before starting it. Readiness is an observation, not proof that
//! the HTTP endpoint belongs to the newly launched process.

use crate::usb::PinnedUsb;
use crate::wda_bundle::validate_runner_bundle_id;
use crate::wda_http::HttpResponse;
use crate::wda_transport::{UsbWda, WDA_PORT};
use idevice::services::dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener};
use idevice::services::installation_proxy::InstallationProxyClient;
use idevice::usbmuxd::errors::UsbmuxdError;
use idevice::{IdeviceError, IdeviceService};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinError, JoinHandle};
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const VACANCY_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

mod native_process;

/// Fixed failure checkpoints; never native output, paths, PIDs or identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeProcessStep {
    TargetPath,
    UsbDiscovery,
    UsbDeadline,
    OutputCreate,
    CommandSpawn,
    CommandDeadline,
    CommandExit,
    MetadataRead,
    MetadataLimit,
    MetadataDecode,
    MetadataContract,
    InventorySchema,
    InventoryPid,
    InventoryPath,
    InventoryAmbiguous,
    IdentityChanged,
    Preexisting,
    MissingAfterStart,
}

impl NativeProcessStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TargetPath => "process_target_path",
            Self::UsbDiscovery => "process_usb_discovery",
            Self::UsbDeadline => "process_usb_deadline",
            Self::OutputCreate => "process_output_create",
            Self::CommandSpawn => "process_command_spawn",
            Self::CommandDeadline => "process_command_deadline",
            Self::CommandExit => "process_command_exit",
            Self::MetadataRead => "process_metadata_read",
            Self::MetadataLimit => "process_metadata_limit",
            Self::MetadataDecode => "process_metadata_decode",
            Self::MetadataContract => "process_metadata_contract",
            Self::InventorySchema => "process_inventory_schema",
            Self::InventoryPid => "process_inventory_pid",
            Self::InventoryPath => "process_inventory_path",
            Self::InventoryAmbiguous => "process_inventory_ambiguous",
            Self::IdentityChanged => "process_identity_changed",
            Self::Preexisting => "process_preexisting",
            Self::MissingAfterStart => "process_missing_after_start",
        }
    }
}

/// Fixed categories only: no upstream error, device identifier, or test log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerError {
    InvalidBundleId,
    InvalidTimeout,
    LookupFailed,
    LookupTimedOut,
    EndpointBusy,
    EndpointUnproven,
    EndpointCleanupUnproven,
    StartupTimedOut,
    PlanStartUnproven,
    /// Initialization failed during or mentioning automation setup; no setting is inferred.
    AutomationInitializationFailed,
    /// The plan stayed in the observed automation wait until the startup deadline.
    AutomationWaitTimedOut,
    InitializationFailed,
    BootstrapFailed,
    RunnerExited,
    RunnerFailed,
    TaskFailed,
    OwnershipLost,
    ProcessIdentityUnproven,
    RunnerStopUnproven,
    NativeProcess(NativeProcessStep),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NativeProcess(step) => f.write_str(step.as_str()),
            // Every other variant is a fixed unit category.
            _ => write!(f, "{self:?}"),
        }
    }
}

/// Owns the host XCTest task, including while startup or shutdown is cancelled.
///
/// Drop requests host cancellation only. `abort_and_join` stops the exact owned
/// native runner once and requires native absence plus pinned endpoint refusal.
#[must_use = "keep the handle alive for the lifetime of the XCTest task"]
pub struct OwnedWdaRunner {
    task: Option<JoinHandle<Result<(), RunnerError>>>,
    usb: Option<PinnedUsb>,
    ownership: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    native: Option<native_process::Identity>,
}

/// Read-only task liveness for a blocking action worker. It cannot cancel the runner.
#[derive(Clone)]
pub struct RunnerLiveness(AbortHandle);

impl RunnerLiveness {
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl OwnedWdaRunner {
    /// Look up one exact installed `.xctrunner` bundle and start it once.
    ///
    /// The timeout must be positive and at most 120 seconds. One absolute deadline
    /// covers discovery, lookup, the test-plan callback, and HTTP readiness. Lookup
    /// additionally has a 15-second time bound. No WDA session is created or adopted.
    /// The ownership callback must retain and validate the caller's ServiceLock;
    /// it remains required through exact native process cleanup.
    /// Call and retain this handle inside a running Tokio runtime.
    pub async fn start(
        usb: PinnedUsb,
        runner_bundle_id: &str,
        startup_timeout: Duration,
        owns: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, RunnerError> {
        validate_runner_bundle_id(runner_bundle_id).map_err(|_| RunnerError::InvalidBundleId)?;
        let deadline = startup_deadline(startup_timeout)?;
        let owns: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(owns);
        if !owns() {
            return Err(RunnerError::OwnershipLost);
        }
        let (provider, config) = lookup_before_deadline(
            async {
                let provider = usb
                    .provider()
                    .await
                    .map_err(|_| RunnerError::LookupFailed)?;
                let mut proxy = InstallationProxyClient::connect(&*provider)
                    .await
                    .map_err(|_| RunnerError::LookupFailed)?;
                let config =
                    TestConfig::from_installation_proxy(&mut proxy, runner_bundle_id, None)
                        .await
                        .map_err(|_| RunnerError::LookupFailed)?;
                require_port_vacant(provider.connect(WDA_PORT), deadline).await?;
                Ok((provider, config))
            },
            deadline,
        )
        .await?;
        if !owns() {
            return Err(RunnerError::OwnershipLost);
        }
        let target = native_process::Target::new(
            usb.expected_id().clone(),
            &config.runner_app_path,
            &config.runner_bundle_executable,
        )?;
        if target
            .observe(&usb, deadline, &*owns, None)
            .await?
            .is_some()
        {
            return Err(RunnerError::NativeProcess(NativeProcessStep::Preexisting));
        }
        let wda = UsbWda::new(usb.clone());
        let launch_owns = Arc::clone(&owns);
        let mut runner = Self::start_task(
            move |mut listener| async move {
                if !launch_owns() {
                    return Err(RunnerError::OwnershipLost);
                }
                XCUITestService::new(provider)
                    .run(config, &mut listener, None)
                    .await
                    .map_err(|_| RunnerError::RunnerFailed)
            },
            || async {
                // The outer absolute deadline also covers USB discovery/connect.
                wda.request("GET", "/status", b"", STATUS_TIMEOUT)
                    .await
                    .ok()
            },
            deadline,
        )
        .await?;
        runner.usb = Some(usb.clone());
        runner.ownership = Some(Arc::clone(&owns));
        let identity = target.observe(&usb, deadline, &*owns, None).await;
        match identity {
            Ok(Some(pid)) => runner.native = Some(native_process::Identity { target, pid }),
            result => {
                let error = result.err().unwrap_or(RunnerError::NativeProcess(
                    NativeProcessStep::MissingAfterStart,
                ));
                let _ = runner.abort_and_join().await;
                return Err(error);
            }
        }
        Ok(runner)
    }

    /// A point-in-time host task observation; it is not an HTTP health check.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn generation(&self) -> Option<tokio::task::Id> {
        self.task.as_ref().map(JoinHandle::id)
    }

    pub fn liveness(&self) -> RunnerLiveness {
        RunnerLiveness(self.task.as_ref().expect("owned task").abort_handle())
    }

    pub(crate) fn write_admission(&self) -> Option<Arc<dyn Fn() -> bool + Send + Sync>> {
        let owns = self.ownership.clone()?;
        let live = RunnerLiveness(self.task.as_ref()?.abort_handle());
        Some(Arc::new(move || owns() && !live.is_finished()))
    }

    /// Join the cancelled host task and observe pinned endpoint refusal within
    /// one deadline. Cancellation keeps the abort guard; it never proves cleanup.
    pub async fn abort_and_join(mut self) -> Result<(), RunnerError> {
        let usb = self.usb.clone();
        let native = self.native.clone();
        let ownership = self.ownership.clone();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let joined = self.join_host(deadline).await;
        let stop = match (&usb, &native, &ownership) {
            (Some(usb), Some(identity), Some(owns)) => {
                identity.stop_once(usb, deadline, &**owns).await
            }
            _ => Err(RunnerError::RunnerStopUnproven),
        };
        // Even an unproven signal acknowledgement must be followed by read-only
        // cleanup observation. It can never authorize a repeated signal.
        let closed = Self::wait_closed(
            move || {
                let usb = usb.clone();
                let native = native.clone();
                let ownership = ownership.clone();
                async move {
                    let usb = usb.ok_or(RunnerError::EndpointCleanupUnproven)?;
                    let identity = native.ok_or(RunnerError::RunnerStopUnproven)?;
                    let owns = ownership.ok_or(RunnerError::OwnershipLost)?;
                    if identity
                        .target
                        .observe(&usb, deadline, &*owns, Some(identity.pid))
                        .await?
                        .is_some()
                    {
                        return Err(RunnerError::EndpointBusy);
                    }
                    endpoint_vacant(&usb).await?;
                    if !owns() {
                        return Err(RunnerError::OwnershipLost);
                    }
                    Ok(())
                }
            },
            deadline,
        )
        .await;
        stop?;
        closed?;
        joined
    }

    #[cfg(test)]
    async fn abort_and_join_with<P, PF>(mut self, probe: P) -> Result<(), RunnerError>
    where
        P: FnMut() -> PF,
        PF: Future<Output = Result<(), RunnerError>>,
    {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let joined = self.join_host(deadline).await;
        Self::wait_closed(probe, deadline).await?;
        joined
    }

    async fn join_host(&mut self, deadline: Instant) -> Result<(), RunnerError> {
        if let Some(task) = self.task.as_mut() {
            task.abort();
            let result = timeout_at(deadline, task).await;
            match result {
                Ok(result) => {
                    self.task.take();
                    match result {
                        Ok(result) => result,
                        Err(error) if error.is_cancelled() => Ok(()),
                        Err(_) => Err(RunnerError::TaskFailed),
                    }
                }
                Err(_) => Err(RunnerError::TaskFailed),
            }
        } else {
            Ok(())
        }
    }

    async fn wait_closed<P, PF>(mut probe: P, deadline: Instant) -> Result<(), RunnerError>
    where
        P: FnMut() -> PF,
        PF: Future<Output = Result<(), RunnerError>>,
    {
        loop {
            if Instant::now() >= deadline {
                return Err(RunnerError::EndpointCleanupUnproven);
            }
            let observed = timeout_at(deadline, probe()).await;
            if Instant::now() >= deadline {
                return Err(RunnerError::EndpointCleanupUnproven);
            }
            match observed {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(RunnerError::EndpointBusy)) => {
                    sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
                }
                Ok(Err(
                    error @ (RunnerError::OwnershipLost
                    | RunnerError::ProcessIdentityUnproven
                    | RunnerError::RunnerStopUnproven
                    | RunnerError::NativeProcess(_)),
                )) => return Err(error),
                _ => return Err(RunnerError::EndpointCleanupUnproven),
            }
        }
    }

    async fn start_task<R, F, P, PF>(
        run: R,
        probe: P,
        deadline: Instant,
    ) -> Result<Self, RunnerError>
    where
        R: FnOnce(PlanListener) -> F,
        F: Future<Output = Result<(), RunnerError>> + Send + 'static,
        P: FnMut() -> PF,
        PF: Future<Output = Option<HttpResponse>>,
    {
        if Instant::now() >= deadline {
            return Err(RunnerError::StartupTimedOut);
        }
        let (phase, receiver) = watch::channel(PlanState::starting());
        // Install the abort-on-drop owner before the first await after spawn.
        let mut runner = Self {
            task: Some(tokio::spawn(run(PlanListener { phase }))),
            usb: None,
            ownership: None,
            native: None,
        };
        if let Err(error) = runner.wait_ready(receiver, probe, deadline).await {
            let _ = runner.abort_and_join().await;
            return Err(error);
        }
        Ok(runner)
    }

    async fn wait_ready<P, PF>(
        &mut self,
        phase: watch::Receiver<PlanState>,
        mut probe: P,
        deadline: Instant,
    ) -> Result<(), RunnerError>
    where
        P: FnMut() -> PF,
        PF: Future<Output = Option<HttpResponse>>,
    {
        let mut begun = phase.clone();
        let mut finished = phase.clone();
        let mut failed = phase.clone();
        let readiness = async {
            begun
                .wait_for(|state| state.phase == PlanPhase::Running)
                .await
                .map_err(|_| RunnerError::RunnerExited)?;
            loop {
                if probe().await.as_ref().is_some_and(strict_ready) {
                    return Ok(());
                }
                sleep(POLL_INTERVAL).await;
            }
        };
        tokio::select! {
            biased;
            failure = async {
                failed.wait_for(|state| state.failure.is_some()).await.ok().and_then(|state| state.failure)
            } => {
                if let Some(error) = failure {
                    return Err(error);
                }
                let result = self.task.as_mut().expect("owned task").await;
                self.task.take();
                return Err(startup_exit_with_state(result, *phase.borrow()));
            }
            _ = sleep_until(deadline) => return Err(startup_timeout_error(*phase.borrow())),
            result = self.task.as_mut().expect("owned task") => {
                self.task.take();
                return Err(startup_exit_with_state(result, *phase.borrow()));
            }
            _ = async { let _ = finished.wait_for(|state| state.phase == PlanPhase::Finished).await; } => {
                return Err(RunnerError::RunnerExited);
            }
            result = readiness => result?,
        }
        // A ready response cannot override an observed deadline or task/plan exit.
        if Instant::now() >= deadline {
            return Err(startup_timeout_error(*phase.borrow()));
        }
        if let Some(error) = phase.borrow().failure {
            return Err(error);
        }
        if self.is_finished() {
            let result = self.task.as_mut().expect("owned task").await;
            self.task.take();
            return Err(startup_exit_with_state(result, *phase.borrow()));
        }
        if phase.borrow().phase != PlanPhase::Running {
            return Err(RunnerError::RunnerExited);
        }
        Ok(())
    }
}

/// Check the pinned device's WDA port without creating a session or runner.
/// A refusal proves vacancy only at this instant; callers must exclude restarts.
pub async fn endpoint_vacant(usb: &PinnedUsb) -> Result<(), RunnerError> {
    let provider = usb
        .provider()
        .await
        .map_err(|_| RunnerError::LookupFailed)?;
    require_port_vacant(
        provider.connect(WDA_PORT),
        Instant::now() + VACANCY_TIMEOUT + Duration::from_secs(1),
    )
    .await
}

impl Drop for OwnedWdaRunner {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn startup_deadline(timeout: Duration) -> Result<Instant, RunnerError> {
    if timeout.is_zero() || timeout > MAX_STARTUP_TIMEOUT {
        return Err(RunnerError::InvalidTimeout);
    }
    Ok(Instant::now() + timeout)
}

async fn lookup_before_deadline<T>(
    lookup: impl Future<Output = Result<T, RunnerError>>,
    deadline: Instant,
) -> Result<T, RunnerError> {
    let lookup_deadline = deadline.min(Instant::now() + LOOKUP_TIMEOUT);
    if Instant::now() >= lookup_deadline {
        return Err(RunnerError::StartupTimedOut);
    }
    let result = timeout_at(lookup_deadline, lookup).await;
    if Instant::now() >= deadline {
        return Err(RunnerError::StartupTimedOut);
    }
    if Instant::now() >= lookup_deadline {
        return Err(RunnerError::LookupTimedOut);
    }
    result.map_err(|_| RunnerError::LookupTimedOut)?
}

async fn require_port_vacant<T>(
    connect: impl Future<Output = Result<T, IdeviceError>>,
    deadline: Instant,
) -> Result<(), RunnerError> {
    if Instant::now() >= deadline {
        return Err(RunnerError::StartupTimedOut);
    }
    let check_deadline = deadline.min(Instant::now() + VACANCY_TIMEOUT);
    let result = timeout_at(check_deadline, connect).await;
    if Instant::now() >= deadline {
        return Err(RunnerError::StartupTimedOut);
    }
    if Instant::now() >= check_deadline {
        return Err(RunnerError::EndpointUnproven);
    }
    match result {
        Ok(Err(IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused))) => Ok(()),
        Ok(Ok(_)) => Err(RunnerError::EndpointBusy),
        _ => Err(RunnerError::EndpointUnproven),
    }
}

fn strict_ready(response: &HttpResponse) -> bool {
    response.status == 200
        && serde_json::from_slice::<serde_json::Value>(&response.body)
            .ok()
            .is_some_and(|json| {
                json.get("error").is_none()
                    && json["value"].get("error").is_none()
                    && json["value"]["ready"] == true
            })
}

fn startup_exit(result: Result<Result<(), RunnerError>, JoinError>) -> RunnerError {
    match result {
        Ok(Ok(())) => RunnerError::RunnerExited,
        Ok(Err(error)) => error,
        Err(_) => RunnerError::TaskFailed,
    }
}

fn startup_exit_with_state(
    result: Result<Result<(), RunnerError>, JoinError>,
    state: PlanState,
) -> RunnerError {
    if let Some(error) = state.failure {
        error
    } else {
        startup_exit(result)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PlanPhase {
    Starting,
    Running,
    Finished,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct PlanState {
    phase: PlanPhase,
    automation_waiting: bool,
    failure: Option<RunnerError>,
}

impl PlanState {
    fn starting() -> Self {
        Self {
            phase: PlanPhase::Starting,
            automation_waiting: false,
            failure: None,
        }
    }
}

fn startup_timeout_error(state: PlanState) -> RunnerError {
    if let Some(error) = state.failure {
        error
    } else if state.phase == PlanPhase::Starting {
        if state.automation_waiting {
            RunnerError::AutomationWaitTimedOut
        } else {
            RunnerError::PlanStartUnproven
        }
    } else {
        RunnerError::StartupTimedOut
    }
}

fn initialization_failure(message: &str, automation_waiting: bool) -> RunnerError {
    if automation_waiting
        || message.contains("enabling automation mode")
        || message.contains("com.apple.LocalAuthentication")
    {
        RunnerError::AutomationInitializationFailed
    } else {
        RunnerError::InitializationFailed
    }
}

fn initialization_diagnostic(message: &str, automation_waiting: bool) -> String {
    const DOMAIN: &str = "com.apple.LocalAuthentication";
    let code = message
        .split_once("Error Domain=com.apple.LocalAuthentication Code=")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .filter(|value| value.len() <= 11)
        .and_then(|value| value.parse::<i32>().ok());
    format!(
        "runner_initialization_failure automation_waiting={automation_waiting} local_authentication={} local_authentication_code={code:?}",
        message.contains(DOMAIN),
    )
}

struct PlanListener {
    phase: watch::Sender<PlanState>,
}

impl XCUITestListener for PlanListener {
    async fn test_runner_ready_with_capabilities(&mut self) -> Result<(), IdeviceError> {
        eprintln!("handkited: runner_startup_stage=runner_ready");
        Ok(())
    }

    async fn did_begin_initializing_for_ui_testing(&mut self) -> Result<(), IdeviceError> {
        eprintln!("handkited: runner_startup_stage=ui_initializing");
        Ok(())
    }

    async fn log_message(&mut self, message: &str) -> Result<(), IdeviceError> {
        self.log_debug_message(message).await
    }

    async fn log_debug_message(&mut self, message: &str) -> Result<(), IdeviceError> {
        if message.contains("Failed to initialize for UI testing") {
            let automation_waiting = self.phase.borrow().automation_waiting;
            eprintln!(
                "handkited: {}",
                initialization_diagnostic(message, automation_waiting)
            );
            self.fail(initialization_failure(message, automation_waiting));
            return Ok(());
        }
        let waiting =
            message.contains("entering wait loop") && message.contains("enabling automation mode");
        let done = message.contains("cleaning up enabling automation mode");
        if waiting || done {
            let changed = self.phase.send_if_modified(|state| {
                if state.failure.is_some() || state.phase == PlanPhase::Finished {
                    return false;
                }
                let next = !done;
                let changed = state.automation_waiting != next;
                state.automation_waiting = next;
                changed
            });
            if changed {
                let stage = if done {
                    "automation_finished"
                } else {
                    "automation_waiting"
                };
                eprintln!("handkited: runner_startup_stage={stage}");
            }
        }
        Ok(())
    }

    async fn did_begin_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        self.phase.send_if_modified(|state| {
            if state.phase == PlanPhase::Starting && state.failure.is_none() {
                state.phase = PlanPhase::Running;
                true
            } else {
                false
            }
        });
        Ok(())
    }

    async fn did_finish_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        self.phase.send_if_modified(|state| {
            if state.failure.is_some() || state.phase == PlanPhase::Finished {
                return false;
            }
            state.phase = PlanPhase::Finished;
            true
        });
        Ok(())
    }

    async fn initialization_for_ui_testing_did_fail(
        &mut self,
        description: &str,
    ) -> Result<(), IdeviceError> {
        let automation_waiting = self.phase.borrow().automation_waiting;
        eprintln!(
            "handkited: {}",
            initialization_diagnostic(description, automation_waiting)
        );
        self.fail(initialization_failure(description, automation_waiting));
        Ok(())
    }

    async fn did_fail_to_bootstrap(&mut self, _description: &str) -> Result<(), IdeviceError> {
        self.fail(RunnerError::BootstrapFailed);
        Err(IdeviceError::UnexpectedResponse(
            "test runner bootstrap failed".into(),
        ))
    }
}

impl PlanListener {
    fn fail(&self, error: RunnerError) {
        self.phase.send_if_modified(|state| {
            if state.failure.is_some() || state.phase == PlanPhase::Finished {
                return false;
            }
            state.failure = Some(error);
            true
        });
    }
}

#[cfg(test)]
mod tests;
