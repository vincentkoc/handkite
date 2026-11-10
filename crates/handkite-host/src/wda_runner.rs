//! Own one XCTest task on an explicitly pinned USB device.
//!
//! This library does not acquire exclusive device ownership. Callers must establish
//! that ownership before starting it. Readiness is an observation, not proof that
//! the HTTP endpoint belongs to the newly launched process.

use crate::usb::PinnedUsb;
use crate::wda_http::HttpResponse;
use crate::wda_transport::UsbWda;
use idevice::services::dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener};
use idevice::services::installation_proxy::InstallationProxyClient;
use idevice::{IdeviceError, IdeviceService};
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Fixed categories only: no upstream error, device identifier, or test log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerError {
    InvalidBundleId,
    InvalidTimeout,
    LookupFailed,
    LookupTimedOut,
    StartupTimedOut,
    RunnerExited,
    RunnerFailed,
    TaskFailed,
}

/// Owns the host XCTest task, including while startup or shutdown is cancelled.
///
/// Drop requests cancellation; use `abort_and_join` to observe task completion.
/// Neither operation proves that the device-side process has terminated.
#[must_use = "keep the handle alive for the lifetime of the XCTest task"]
pub struct OwnedWdaRunner {
    task: Option<JoinHandle<Result<(), RunnerError>>>,
}

impl OwnedWdaRunner {
    /// Look up one exact installed `.xctrunner` bundle and start it once.
    ///
    /// The timeout must be positive and at most 120 seconds. One absolute deadline
    /// covers discovery, lookup, the test-plan callback, and HTTP readiness. Lookup
    /// additionally has a 15-second time bound. No WDA session is created or adopted.
    /// Call and retain this handle inside a running Tokio runtime.
    pub async fn start(
        usb: PinnedUsb,
        runner_bundle_id: &str,
        startup_timeout: Duration,
    ) -> Result<Self, RunnerError> {
        validate_bundle_id(runner_bundle_id)?;
        let deadline = startup_deadline(startup_timeout)?;
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
                Ok((provider, config))
            },
            deadline,
        )
        .await?;
        let wda = UsbWda::new(usb);
        Self::start_task(
            move |mut listener| async move {
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
        .await
    }

    /// A point-in-time host task observation; it is not an HTTP health check.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Request cancellation and join the host task without detaching on cancellation.
    pub async fn abort_and_join(mut self) -> Result<(), RunnerError> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        task.abort();
        let result = task.await;
        self.task.take();
        match result {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(_) => Err(RunnerError::TaskFailed),
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
        let (phase, receiver) = watch::channel(PlanPhase::Starting);
        // Install the abort-on-drop owner before the first await after spawn.
        let mut runner = Self {
            task: Some(tokio::spawn(run(PlanListener { phase }))),
        };
        if let Err(error) = runner.wait_ready(receiver, probe, deadline).await {
            let _ = runner.abort_and_join().await;
            return Err(error);
        }
        Ok(runner)
    }

    async fn wait_ready<P, PF>(
        &mut self,
        phase: watch::Receiver<PlanPhase>,
        mut probe: P,
        deadline: Instant,
    ) -> Result<(), RunnerError>
    where
        P: FnMut() -> PF,
        PF: Future<Output = Option<HttpResponse>>,
    {
        let mut begun = phase.clone();
        let mut finished = phase.clone();
        let readiness = async {
            begun
                .wait_for(|phase| *phase == PlanPhase::Running)
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
            _ = sleep_until(deadline) => return Err(RunnerError::StartupTimedOut),
            result = self.task.as_mut().expect("owned task") => {
                self.task.take();
                return Err(startup_exit(result));
            }
            _ = finished.wait_for(|phase| *phase == PlanPhase::Finished) => {
                return Err(RunnerError::RunnerExited);
            }
            result = readiness => result?,
        }
        // A ready response cannot override an observed deadline or task/plan exit.
        if Instant::now() >= deadline {
            return Err(RunnerError::StartupTimedOut);
        }
        if self.is_finished() {
            let result = self.task.as_mut().expect("owned task").await;
            self.task.take();
            return Err(startup_exit(result));
        }
        if *phase.borrow() != PlanPhase::Running {
            return Err(RunnerError::RunnerExited);
        }
        Ok(())
    }
}

impl Drop for OwnedWdaRunner {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn validate_bundle_id(bundle_id: &str) -> Result<(), RunnerError> {
    if bundle_id.len() > 255
        || !bundle_id.ends_with(".xctrunner")
        || bundle_id.split('.').count() < 3
        || !bundle_id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(RunnerError::InvalidBundleId);
    }
    Ok(())
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum PlanPhase {
    Starting,
    Running,
    Finished,
}

struct PlanListener {
    phase: watch::Sender<PlanPhase>,
}

impl XCUITestListener for PlanListener {
    async fn did_begin_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        self.phase.send_if_modified(|phase| {
            if *phase == PlanPhase::Starting {
                *phase = PlanPhase::Running;
                true
            } else {
                false
            }
        });
        Ok(())
    }

    async fn did_finish_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        self.phase.send_replace(PlanPhase::Finished);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
