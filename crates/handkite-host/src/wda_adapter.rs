use crate::native_lock;
use crate::usb::PinnedUsb;
use crate::wda_action::{WdaActions, WdaRequest};
use crate::wda_http::HttpResponse;
use crate::wda_runner::{OwnedWdaRunner, RunnerError, RunnerLiveness};
use crate::wda_session::{
    SessionClose, close_owned_session, create_owned_session, restore_owned_idle_settings,
    start_owned_idle_settings,
};
use crate::wda_settings::validate_session;
use crate::wda_settings::{IdleSettingsLease, Restoration};
use crate::wda_transport::UsbWda;
use base64::Engine;
use handkite_core::controller::{
    ActionAdmission, AdapterDiagnostic, AdapterError, DeviceAdapter, RawImageObservation,
    RawObservation,
};
use handkite_core::protocol::{
    Action, DeviceState, HardwareId, ImageFormat, LockState, Motion, ObservationWindow, Point,
    ScreenImage, TransportReceipt,
};
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod cache;
mod display;
mod focused_text;
mod measurement;
mod navigation;
mod ocr;
mod targets;
#[cfg(test)]
pub(crate) use targets::dispatch_native as test_dispatch_native;
mod targets_pixels;
pub use measurement::CaptureTimings;
use measurement::{Stage, measure};

pub(crate) const SCREEN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,index,traits,hittable,customActions,nativeFrame";
const ATTRIBUTE_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const HOME_LAUNCH_AGE: Duration = Duration::from_secs(2);
const HOME_COMPLETION_BUDGET: Duration = Duration::from_secs(6);
const HOME_PROOF_TAIL: Duration = Duration::from_secs(1);
const TEXT_INPUT_AGE: Duration = Duration::from_secs(5);

async fn before_input_deadline<T>(
    until: Instant,
    work: impl Future<Output = Result<T, AdapterError>>,
) -> Result<T, AdapterError> {
    let remaining = until
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(AdapterError)?;
    let result = tokio::time::timeout(remaining, work)
        .await
        .map_err(|_| AdapterError)?;
    if Instant::now() >= until {
        return Err(AdapterError);
    }
    result
}

struct CapturePayload {
    tree: Option<String>,
    app: String,
    reported: Option<bool>,
    screenshot: ScreenImage,
    finished: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureFailure {
    Adapter,
    ImageDrift,
}

impl From<AdapterError> for CaptureFailure {
    fn from(_: AdapterError) -> Self {
        Self::Adapter
    }
}

impl From<CaptureFailure> for AdapterError {
    fn from(_: CaptureFailure) -> Self {
        Self
    }
}

pub struct WdaAdapter {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: Option<tokio::runtime::Runtime>,
    actions: WdaActions,
    owned_runner: Option<OwnedWdaRunner>,
    owned_session: Option<String>,
    owned_idle: Option<IdleSettingsLease>,
    launch_admission: Option<(String, Instant)>,
    cache: cache::Cache,
    error_diagnostic: Option<AdapterDiagnostic>,
    focused_text: Option<focused_text::Input>,
    text_token: u64,
    text_response_poisoned: bool,
    targets: Option<targets::Targets>,
    ocr_cache: ocr::Cache,
    target_snapshot: Option<targets::Snapshot>,
    target_input: Option<targets::Input>,
    target_readback: Option<targets::Input>,
    target_delta: Option<targets::DeltaBaseline>,
    pending_effect: Option<targets::PendingEffect>,
    effect_reports: Vec<handkite_core::controller::TargetEffectReport>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedStartupError {
    ShutdownRequested,
    RuntimeUnavailable,
    Runner(RunnerError),
    SessionUnavailable,
    IdleSettingsUnavailable,
    IdleRestorationUnproven,
}

impl OwnedStartupError {
    pub fn message(self) -> &'static str {
        if self == Self::Runner(RunnerError::PairingRequired) {
            crate::wda_runner::PAIRING_REQUIRED_MESSAGE
        } else if self == Self::Runner(RunnerError::AuthorizationRequired) {
            crate::wda_runner::AUTHORIZATION_REQUIRED_MESSAGE
        } else {
            self.category()
        }
    }

    pub fn category(self) -> &'static str {
        match self {
            Self::ShutdownRequested => "shutdown_requested",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::Runner(error) => match error {
                RunnerError::InvalidBundleId => "runner_invalid_bundle_id",
                RunnerError::InvalidTimeout => "runner_invalid_timeout",
                RunnerError::LookupFailed => "runner_lookup_failed",
                RunnerError::LookupTimedOut => "runner_lookup_timed_out",
                RunnerError::PairingRequired => "pairing_required",
                RunnerError::AuthorizationRequired => "authorization_required",
                RunnerError::EndpointBusy => "runner_endpoint_busy",
                RunnerError::EndpointUnproven => "runner_endpoint_unproven",
                RunnerError::EndpointCleanupUnproven => "runner_endpoint_cleanup_unproven",
                RunnerError::StartupTimedOut => "runner_startup_timed_out",
                RunnerError::PlanStartUnproven => "runner_plan_start_unproven",
                RunnerError::AutomationWaitTimedOut => "automation_wait_timed_out",
                RunnerError::AutomationInitializationFailed => "automation_initialization_failed",
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
            },
            Self::SessionUnavailable => "session_unavailable",
            Self::IdleSettingsUnavailable => "session_idle_settings_unavailable",
            Self::IdleRestorationUnproven => "session_idle_restoration_unproven",
        }
    }
}

impl WdaAdapter {
    /// Observe the owned host task without transferring or cancelling its ownership.
    pub fn runner_liveness(&self) -> Option<RunnerLiveness> {
        self.owned_runner.as_ref().map(OwnedWdaRunner::liveness)
    }

    pub fn new(expected: HardwareId) -> Result<Self, AdapterError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AdapterError)?;
        Ok(Self {
            usb: PinnedUsb::new(expected.clone()),
            expected,
            runtime: Some(runtime),
            actions: WdaActions::default(),
            error_diagnostic: None,
            owned_runner: None,
            owned_session: None,
            owned_idle: None,
            launch_admission: None,
            cache: cache::Cache::default(),
            focused_text: None,
            text_token: 0,
            text_response_poisoned: false,
            targets: None,
            ocr_cache: ocr::Cache::default(),
            target_snapshot: None,
            target_input: None,
            target_readback: None,
            target_delta: None,
            pending_effect: None,
            effect_reports: Vec::new(),
        })
    }

    /// Start one configured XCTest runner and create one exact WDA session.
    /// The caller must first obtain exclusive device and WDA ownership.
    /// The multi-thread runtime continues driving XCTest while the socket server is idle.
    pub fn new_owned(
        expected: HardwareId,
        runner_bundle_id: &str,
        should_stop: impl Fn() -> bool,
        owns: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, OwnedStartupError> {
        if should_stop() {
            return Err(OwnedStartupError::ShutdownRequested);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|_| OwnedStartupError::RuntimeUnavailable)?;
        let usb = PinnedUsb::new(expected.clone());
        let runner = runtime.block_on(run_until_shutdown(
            async {
                OwnedWdaRunner::start(
                    usb.clone(),
                    runner_bundle_id,
                    Duration::from_secs(120),
                    owns,
                )
                .await
                .map_err(OwnedStartupError::Runner)
            },
            &should_stop,
            OwnedStartupError::ShutdownRequested,
        ));
        let runner = match runner {
            Ok(runner) => runner,
            Err(error) => {
                runtime.shutdown_timeout(Duration::from_secs(5));
                return Err(error);
            }
        };
        let mut adapter = Self {
            expected,
            usb,
            runtime: Some(runtime),
            actions: WdaActions::default(),
            owned_runner: Some(runner),
            owned_session: None,
            owned_idle: None,
            launch_admission: None,
            cache: cache::Cache::default(),
            error_diagnostic: None,
            focused_text: None,
            text_token: 0,
            text_response_poisoned: false,
            targets: None,
            ocr_cache: ocr::Cache::default(),
            target_snapshot: None,
            target_input: None,
            target_readback: None,
            target_delta: None,
            pending_effect: None,
            effect_reports: Vec::new(),
        };
        let liveness = adapter
            .owned_runner
            .as_ref()
            .expect("owned runner")
            .liveness();
        let session = adapter
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(create_owned_session(
                &UsbWda::new(adapter.usb.clone()),
                || {
                    if should_stop() || liveness.is_finished() {
                        Err(())
                    } else {
                        Ok(())
                    }
                },
            ))
            .map_err(|_| OwnedStartupError::SessionUnavailable)?;
        adapter.owned_session = Some(session.clone());
        adapter.actions = WdaActions::bound_to_existing(&session)
            .map_err(|_| OwnedStartupError::SessionUnavailable)?;
        if should_stop() || liveness.is_finished() {
            return Err(OwnedStartupError::SessionUnavailable);
        }
        let settings_wda = UsbWda::new(adapter.usb.clone()).with_write_admission(Some(
            adapter
                .owned_runner
                .as_ref()
                .expect("owned runner")
                .write_admission()
                .ok_or(OwnedStartupError::IdleSettingsUnavailable)?,
        ));
        adapter.owned_idle = Some(
            adapter
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(start_owned_idle_settings(&settings_wda, &session))
                .map_err(|(_, restoration)| match restoration {
                    Restoration::Unproven(_) => OwnedStartupError::IdleRestorationUnproven,
                    _ => OwnedStartupError::IdleSettingsUnavailable,
                })?,
        );
        if should_stop() || liveness.is_finished() {
            return Err(OwnedStartupError::ShutdownRequested);
        }
        Ok(adapter)
    }

    fn validate_owned(&self) -> Result<(), AdapterError> {
        self.validate_owned_with_deadline(None)
    }

    fn validate_owned_with_deadline(&self, deadline: Option<Instant>) -> Result<(), AdapterError> {
        let result = self.validate_owned_inner(deadline);
        if result.is_err() {
            self.cache.clear();
        }
        result
    }

    fn cache_key(&self) -> Option<cache::Key> {
        let runner = self.owned_runner.as_ref()?;
        if runner.is_finished() {
            return None;
        }
        let session = self.owned_session.as_ref()?;
        if !self.actions.prepared_session_matches(session) {
            return None;
        }
        Some(cache::Key {
            device: self.expected.clone(),
            session: session.clone(),
            runner: runner.generation()?,
        })
    }

    fn native_fresh(&self, deadline: Option<Instant>) -> LockState {
        self.cache
            .refresh_native(self.cache_key(), Instant::now(), || match deadline {
                Some(until) => native_lock::query_until(&self.expected, until).state,
                None => native_lock::query(&self.expected).state,
            })
    }

    fn native_hint(&self) -> LockState {
        let key = self.cache_key();
        self.cache
            .native(key.as_ref(), Instant::now())
            .unwrap_or_else(|| self.native_fresh(None))
    }

    fn validate_owned_inner(&self, deadline: Option<Instant>) -> Result<(), AdapterError> {
        let _timing = crate::timing::Span::new("owned_session_checks");
        if self.text_response_poisoned {
            return Err(AdapterError);
        }
        if deadline.is_some_and(|until| Instant::now() >= until) {
            return Err(AdapterError);
        }
        let Some(session) = &self.owned_session else {
            return Ok(());
        };
        if self
            .owned_runner
            .as_ref()
            .is_none_or(OwnedWdaRunner::is_finished)
            || !self.actions.prepared_session_matches(session)
        {
            return Err(AdapterError);
        }
        self.runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(async {
                let wda = UsbWda::new(self.usb.clone());
                let validation = async {
                    validate_session(&wda, session)
                        .await
                        .map_err(|_| AdapterError)
                };
                match deadline {
                    Some(until) => before_input_deadline(until, validation).await,
                    None => validation.await,
                }
            })?;
        if self
            .owned_runner
            .as_ref()
            .is_none_or(OwnedWdaRunner::is_finished)
        {
            return Err(AdapterError);
        }
        Ok(())
    }

    fn capture_checked(
        &self,
        source_path: &str,
    ) -> Result<(RawObservation, Instant), AdapterError> {
        self.capture_checked_with_lock_report(source_path)
            .map(|(observation, captured_at, _)| (observation, captured_at))
    }

    fn capture_checked_with_lock_report(
        &self,
        source_path: &str,
    ) -> Result<(RawObservation, Instant, Option<bool>), AdapterError> {
        self.capture_checked_diagnosed(source_path)
            .map_err(Into::into)
    }

    fn capture_checked_diagnosed(
        &self,
        source_path: &str,
    ) -> Result<(RawObservation, Instant, Option<bool>), CaptureFailure> {
        let result = if crate::timing::enabled() {
            let started = Instant::now();
            let mut timings = CaptureTimings::default();
            let result = self.capture_checked_recorded(source_path, Some(&mut timings));
            timings.finish(started, result.is_ok());
            crate::timing::capture(timings);
            result
        } else {
            self.capture_checked_recorded(source_path, None)
        };
        if !result.as_ref().is_ok_and(|(raw, _, reported)| {
            *reported == Some(false) && raw.state.lock == LockState::Unlocked
        }) {
            self.cache.clear();
        }
        if source_path == SCREEN_SOURCE {
            if let Ok((raw, captured, reported)) = &result {
                self.cache
                    .record_full(self.cache_key(), raw, *captured, *reported);
            }
        } else {
            self.cache.clear_full();
        }
        result
    }

    fn capture_for_action(&self, allow_reuse: bool) -> Result<CheckedCapture, AdapterError> {
        if allow_reuse {
            let key = self.cache_key();
            if let Some(previous) = self.cache.take_full(key.as_ref(), Instant::now()) {
                let started = Instant::now();
                let mut timings = crate::timing::enabled().then(CaptureTimings::default);
                let result = {
                    let _probe = crate::timing::Span::new("tree_cache_probe");
                    self.capture_frame(timings.as_mut())
                };
                let current = match result {
                    Ok(frame) => frame,
                    Err(error) => {
                        self.cache.clear();
                        return Err(error);
                    }
                };
                let same = previous.matches(
                    &current.screenshot,
                    &current.app,
                    current.reported,
                    Instant::now(),
                );
                crate::timing::outcome("tree_cache_hit", same);
                if same {
                    if let Some(mut timings) = timings {
                        timings.mark_reused_tree();
                        timings.finish(started, true);
                        crate::timing::capture(timings);
                    }
                    // Both tree and image retain the original capture window.
                    return Ok((previous.raw, previous.captured_at, previous.reported));
                }
                self.cache.clear();
            }
        } else {
            self.cache.clear_full();
        }
        self.capture_checked_with_lock_report(SCREEN_SOURCE)
    }

    fn capture_frame(
        &self,
        mut timings: Option<&mut CaptureTimings>,
    ) -> Result<CapturePayload, AdapterError> {
        measure(
            timings.as_deref_mut(),
            Stage::OwnershipBefore,
            &Instant::now,
            |_| 0,
            || self.validate_owned(),
        )?;
        let frame =
            self.runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(Self::capture_payload(
                    &UsbWda::new(self.usb.clone()),
                    None,
                    &Instant::now,
                    timings.as_deref_mut(),
                ))?;
        measure(
            timings,
            Stage::OwnershipAfter,
            &Instant::now,
            |_| 0,
            || self.validate_owned(),
        )?;
        Ok(frame)
    }

    /// One normal capture, with no output or input and no change to ownership checks.
    pub fn observe_with_timings(
        &mut self,
    ) -> (Result<RawObservation, AdapterError>, CaptureTimings) {
        let mut timings = CaptureTimings::default();
        let started = Instant::now();
        let result = self
            .finish_pending_effect(true)
            .and_then(|_| {
                self.capture_checked_recorded(SCREEN_SOURCE, Some(&mut timings))
                    .map_err(AdapterError::from)
            })
            .map(|(observation, _, _)| observation)
            .map_err(Into::into);
        timings.finish(started, result.is_ok());
        (result, timings)
    }

    fn capture_checked_recorded(
        &self,
        source_path: &str,
        mut timings: Option<&mut CaptureTimings>,
    ) -> Result<(RawObservation, Instant, Option<bool>), CaptureFailure> {
        measure(
            timings.as_deref_mut(),
            Stage::OwnershipBefore,
            &Instant::now,
            |_| 0,
            || self.validate_owned(),
        )?;
        let captured = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(Self::capture(
                &self.usb,
                &self.expected,
                source_path,
                timings.as_deref_mut(),
            ));
        // Image drift can select a read-only fallback only after ownership has
        // also survived the capture. Other errors keep their original refusal.
        if captured.is_ok() || matches!(captured, Err(CaptureFailure::ImageDrift)) {
            measure(
                timings,
                Stage::OwnershipAfter,
                &Instant::now,
                |_| 0,
                || self.validate_owned(),
            )?;
        }
        captured
    }

    async fn capture(
        usb: &PinnedUsb,
        expected: &HardwareId,
        source_path: &str,
        timings: Option<&mut CaptureTimings>,
    ) -> Result<(RawObservation, Instant, Option<bool>), CaptureFailure> {
        let started_unix_ms = unix_ms()?;
        let wda = UsbWda::new(usb.clone());
        Self::capture_using(
            &wda,
            expected,
            source_path,
            started_unix_ms,
            &Instant::now,
            &unix_ms,
            timings,
        )
        .await
    }

    async fn capture_using(
        wda: &impl WdaRequest,
        expected: &HardwareId,
        source_path: &str,
        started_unix_ms: u64,
        clock: &impl Fn() -> Instant,
        wall: &impl Fn() -> Result<u64, AdapterError>,
        timings: Option<&mut CaptureTimings>,
    ) -> Result<(RawObservation, Instant, Option<bool>), CaptureFailure> {
        let payload = Self::capture_payload(wda, Some(source_path), clock, timings).await?;
        let tree = payload.tree.ok_or(AdapterError)?;
        let lock = classify_lock(&payload.app, &tree, payload.reported);
        Ok((
            RawObservation {
                connected_usb: expected.clone(),
                wda_target: expected.clone(),
                window: ObservationWindow {
                    started_unix_ms,
                    finished_unix_ms: wall()?,
                },
                state: DeviceState {
                    lock,
                    wda_ready: true,
                    foreground_app: Some(payload.app),
                },
                accessibility_tree: tree,
                screenshot: payload.screenshot,
            },
            payload.finished,
            payload.reported,
        ))
    }

    async fn capture_payload(
        wda: &impl WdaRequest,
        source_path: Option<&str>,
        clock: &impl Fn() -> Instant,
        timings: Option<&mut CaptureTimings>,
    ) -> Result<CapturePayload, CaptureFailure> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let result = tokio::time::timeout_at(
            deadline,
            Self::capture_payload_inner(wda, source_path, clock, timings),
        )
        .await
        .map_err(|_| AdapterError)?;
        if tokio::time::Instant::now() >= deadline {
            return Err(CaptureFailure::Adapter);
        }
        result
    }

    async fn capture_payload_inner(
        wda: &impl WdaRequest,
        source_path: Option<&str>,
        clock: &impl Fn() -> Instant,
        mut timings: Option<&mut CaptureTimings>,
    ) -> Result<CapturePayload, CaptureFailure> {
        let enabled = timings.is_some();
        // The source request must finish between these two image captures.
        // Concurrent image/source requests can otherwise describe different UI states.
        let before = if source_path.is_some() {
            let ((response, image_timing), (app, app_timing), (lock, lock_timing)) = tokio::join!(
                measurement::request(
                    enabled,
                    Stage::ScreenshotBeforeRequest,
                    clock,
                    wda.request("GET", "/screenshot", b"", Duration::from_secs(20)),
                ),
                measurement::request(
                    enabled,
                    Stage::AppBeforeRequest,
                    clock,
                    wda.request("GET", "/wda/activeAppInfo", b"", Duration::from_secs(10)),
                ),
                measurement::request(
                    enabled,
                    Stage::LockBeforeRequest,
                    clock,
                    wda.request("GET", "/wda/locked", b"", Duration::from_secs(10)),
                ),
            );
            if let Some(timings) = timings.as_deref_mut() {
                timings.append([image_timing, app_timing, lock_timing]);
            }
            let response = response.map_err(|_| AdapterError)?;
            let image = measure(
                timings.as_deref_mut(),
                Stage::ScreenshotBeforeJson,
                clock,
                String::len,
                || {
                    let value = response_value(response)?;
                    value
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or(AdapterError)
                },
            )?;
            let app = measure(
                timings.as_deref_mut(),
                Stage::AppBeforeJson,
                clock,
                String::len,
                || {
                    let value = response_value(app.map_err(|_| AdapterError)?)?;
                    value["bundleId"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or(AdapterError)
                },
            )?;
            let lock = measure(
                timings.as_deref_mut(),
                Stage::LockBeforeJson,
                clock,
                |_| 0,
                || response_value(lock.map_err(|_| AdapterError)?).map(|v| v.as_bool()),
            )?;
            Some((image, app, lock))
        } else {
            None
        };
        let source = if let Some(path) = source_path {
            let (response, timing) = measurement::request(
                enabled,
                Stage::SourceRequest,
                clock,
                wda.request("GET", path, b"", Duration::from_secs(20)),
            )
            .await;
            (response.map(Some), timing)
        } else {
            (Ok(None), None)
        };
        let (
            (image, image_timing),
            (source, source_timing),
            (app, app_timing),
            (reported_lock, lock_timing),
        ) = tokio::join!(
            measurement::request(
                enabled,
                Stage::ScreenshotRequest,
                clock,
                wda.request("GET", "/screenshot", b"", Duration::from_secs(20))
            ),
            async { source },
            measurement::request(
                enabled,
                Stage::AppRequest,
                clock,
                wda.request("GET", "/wda/activeAppInfo", b"", Duration::from_secs(10))
            ),
            measurement::request(
                enabled,
                Stage::LockRequest,
                clock,
                wda.request("GET", "/wda/locked", b"", Duration::from_secs(10))
            ),
        );
        if let Some(timings) = timings.as_deref_mut() {
            timings.append([image_timing, source_timing, app_timing, lock_timing]);
        }
        let mut decode = |response: Result<HttpResponse, crate::wda_transport::WdaError>, stage| {
            let response = response.map_err(|_| AdapterError)?;
            let bytes = response.body.len();
            measure(
                timings.as_deref_mut(),
                stage,
                clock,
                |_| bytes,
                || response_value(response),
            )
        };
        let image = decode(image, Stage::ScreenshotJson)?;
        let source = match source.map_err(|_| AdapterError)? {
            Some(response) => Some(decode(Ok(response), Stage::SourceJson)?),
            None => None,
        };
        let app = decode(app, Stage::AppJson)?;
        let reported_lock = decode(reported_lock, Stage::LockJson)?;
        let screenshot = measure(
            timings.as_deref_mut(),
            Stage::ImageDecode,
            clock,
            Vec::len,
            || {
                let encoded = image.as_str().ok_or(AdapterError)?;
                let screenshot = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| AdapterError)?;
                if !screenshot.starts_with(b"\x89PNG\r\n\x1a\n") {
                    return Err(AdapterError);
                }
                Ok(screenshot)
            },
        )?;
        let (tree, app, reported_lock) = measure(
            timings.as_deref_mut(),
            Stage::PayloadValidation,
            clock,
            |(tree, _, _): &(Option<String>, String, Option<bool>)| {
                tree.as_ref().map_or(0, String::len)
            },
            || {
                let tree = match source {
                    Some(serde_json::Value::String(tree)) if !tree.is_empty() => Some(tree),
                    None => None,
                    _ => return Err(AdapterError),
                };
                let app = app["bundleId"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or(AdapterError)?
                    .to_owned();
                let reported_lock = reported_lock.as_bool();
                Ok((tree, app, reported_lock))
            },
        )?;
        if let Some((before_image, before_app, before_lock)) = before {
            measure(
                timings,
                Stage::CaptureCoherence,
                clock,
                |_| 0,
                || {
                    if app != before_app || reported_lock != before_lock {
                        return Err(CaptureFailure::Adapter);
                    }
                    if image.as_str() != Some(before_image.as_str()) {
                        // Invalid image data or unknown lock state is not a
                        // recoverable image-drift result.
                        let before_pixels = base64::engine::general_purpose::STANDARD
                            .decode(before_image)
                            .map_err(|_| CaptureFailure::Adapter)?;
                        if before_lock.is_none() || !before_pixels.starts_with(b"\x89PNG\r\n\x1a\n")
                        {
                            return Err(CaptureFailure::Adapter);
                        }
                        return Err(CaptureFailure::ImageDrift);
                    }
                    Ok(())
                },
            )?;
        }
        Ok(CapturePayload {
            tree,
            app,
            reported: reported_lock,
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: screenshot,
            },
            finished: clock(),
        })
    }

    /// Diagnostic capture retaining visibility, enabled and selected attributes.
    /// The monotonic completion anchor bounds later diagnostic input freshness.
    /// Ordinary controller observations continue to use the smaller projection.
    pub fn observe_with_attributes(&mut self) -> Result<(RawObservation, Instant), AdapterError> {
        self.finish_pending_effect(true)?;
        self.capture_checked(ATTRIBUTE_SOURCE)
    }

    /// Diagnostic-only raw lock reply paired with the same image and tree capture.
    /// Callers must combine it with fresh native state before authorizing input.
    pub fn observe_with_attributes_and_lock_report(
        &mut self,
    ) -> Result<(RawObservation, Instant, Option<bool>), AdapterError> {
        self.finish_pending_effect(true)?;
        self.capture_checked_with_lock_report(ATTRIBUTE_SOURCE)
    }

    /// Paired diagnostic capture with the PIN parser's visible/enabled attributes.
    pub fn observe_pin_with_attributes(
        &mut self,
    ) -> Result<(RawObservation, Instant), AdapterError> {
        self.finish_pending_effect(true)?;
        self.capture_checked(PIN_SOURCE)
    }
}

async fn run_until_shutdown<T, E: Copy>(
    work: impl Future<Output = Result<T, E>>,
    should_stop: &impl Fn() -> bool,
    stop_error: E,
) -> Result<T, E> {
    tokio::select! {
        result = work => result,
        _ = async {
            while !should_stop() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => Err(stop_error),
    }
}

impl DeviceAdapter for WdaAdapter {
    fn settle_target_effects(&mut self) -> Result<(), AdapterError> {
        self.finish_pending_effect(true)
    }
    fn settle_target_effects_for(&mut self, target_id: &str) -> Result<(), AdapterError> {
        let rect = self.target_rect(target_id);
        self.settle_for_target(rect)
    }
    fn observe_targets_since(
        &mut self,
        revision: Option<handkite_core::protocol::Revision>,
    ) -> Result<
        (
            RawObservation,
            Vec<handkite_core::controller::ElementTarget>,
        ),
        AdapterError,
    > {
        if let Some(revision) = revision
            && let Some(result) = self.capture_target_delta(revision)?
        {
            return Ok(result);
        }
        self.capture_targets()
    }
    fn target_effect_updates(&mut self) -> Vec<handkite_core::controller::TargetEffectReport> {
        let _ = self.finish_pending_effect(false);
        self.effect_reports.clone()
    }
    fn observe_navigation(&mut self) -> Result<RawObservation, AdapterError> {
        self.finish_pending_effect(true)?;
        self.navigation_frame()
    }
    fn prepare_navigation(
        &mut self,
        action: &Action,
    ) -> Result<(RawObservation, ActionAdmission), AdapterError> {
        self.finish_pending_effect(true)?;
        self.navigation_prepare(action)
    }
    fn execute_navigation(
        &mut self,
        action: &Action,
        deadline: Instant,
        admission: ActionAdmission,
        app: &str,
    ) -> Result<TransportReceipt, AdapterError> {
        self.navigation_execute(action, deadline, admission, app)
    }
    fn requires_current_text_preparation(&self) -> bool {
        self.owned_runner.is_some() || self.owned_session.is_some()
    }

    fn prepare_current_text(
        &mut self,
        action: &Action,
        baseline: &handkite_core::protocol::Observation,
    ) -> Result<Option<handkite_core::controller::PreparedText>, AdapterError> {
        self.finish_pending_effect(true)?;
        self.prepare_focus(action, baseline)
            .inspect_err(|_| self.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture))
    }

    fn execute_prepared_text(
        &mut self,
        action: &Action,
        token: u64,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_focus(action, token, deadline, None)
    }

    fn execute_prepared_text_with_motion(
        &mut self,
        action: &Action,
        token: u64,
        deadline: Instant,
        motion: Option<Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_focus(action, token, deadline, motion)
    }

    fn observe_after_prepared_text(
        &mut self,
        token: u64,
    ) -> Result<handkite_core::controller::TextReadback, AdapterError> {
        self.read_focus(token)
    }

    fn take_error_diagnostic(&mut self) -> Option<AdapterDiagnostic> {
        self.error_diagnostic.take()
    }

    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        self.error_diagnostic = None;
        self.validate_owned().inspect_err(|_| {
            self.error_diagnostic = Some(AdapterDiagnostic::IdentityCheck);
        })?;
        self.runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(self.usb.provider())
            .map_err(|_| {
                self.error_diagnostic = Some(AdapterDiagnostic::IdentityCheck);
                AdapterError
            })?;
        Ok((self.expected.clone(), self.expected.clone()))
    }

    fn observe_targets(
        &mut self,
    ) -> Result<
        (
            RawObservation,
            Vec<handkite_core::controller::ElementTarget>,
        ),
        AdapterError,
    > {
        self.capture_targets()
    }
    fn bind_targets(
        &mut self,
        revision: handkite_core::protocol::Revision,
    ) -> Result<(), AdapterError> {
        self.bind_native_targets(revision)
    }
    fn prepare_target(
        &mut self,
        revision: handkite_core::protocol::Revision,
        action: &handkite_core::controller::ElementAction,
        baseline: &handkite_core::protocol::Observation,
    ) -> Result<handkite_core::controller::PreparedTarget, AdapterError> {
        self.prepare_native_target(revision, action, baseline)
    }
    fn execute_target(
        &mut self,
        action: &handkite_core::controller::ElementAction,
        token: u64,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.send_native_target(action, token, deadline)
    }
    fn target_readback(
        &mut self,
        token: u64,
    ) -> Result<handkite_core::controller::TextReadback, AdapterError> {
        self.read_target_effect(token)
    }
    fn complete_target(&mut self, token: u64) -> handkite_core::controller::TargetCompletion {
        self.finish_target_capture(token)
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.finish_pending_effect(true)?;
        self.error_diagnostic = None;
        self.capture_checked_diagnosed(SCREEN_SOURCE)
            .map(|(observation, _, _)| observation)
            .map_err(|error| {
                if error == CaptureFailure::ImageDrift {
                    self.error_diagnostic = Some(AdapterDiagnostic::CaptureImageDrift);
                }
                AdapterError
            })
    }

    fn observe_image(&mut self) -> Result<RawImageObservation, AdapterError> {
        self.error_diagnostic = None;
        self.cache.clear_full();
        let started = Instant::now();
        let started_unix_ms = unix_ms()?;
        let mut timings = crate::timing::enabled().then(CaptureTimings::default);
        let result = self.capture_frame(timings.as_mut()).and_then(|frame| {
            if frame.reported != Some(false) {
                self.cache.clear();
            }
            Ok(RawImageObservation {
                connected_usb: self.expected.clone(),
                wda_target: self.expected.clone(),
                window: ObservationWindow {
                    started_unix_ms,
                    finished_unix_ms: unix_ms()?,
                },
                screenshot: frame.screenshot,
            })
        });
        if result.is_err() {
            self.cache.clear();
        }
        if let Some(mut timings) = timings {
            timings.finish(started, result.is_ok());
            crate::timing::capture(timings);
        }
        result
    }

    fn observe_after_action(
        &mut self,
        action: &Action,
        admission: Option<ActionAdmission>,
        receipt: &TransportReceipt,
    ) -> Result<RawObservation, AdapterError> {
        if !matches!(action, Action::Home)
            || !matches!(admission, Some(ActionAdmission::NativeHome { .. }))
            || receipt.accepted != Some(true)
        {
            return self.observe();
        }
        let _confirmation = crate::timing::Span::new("home_confirmation");
        let until = Instant::now() + HOME_COMPLETION_BUDGET;
        let proof_until = until + HOME_PROOF_TAIL;
        let result = wait_for_home(until, proof_until, &Instant::now, |until, proof_until| {
            let _attempt = crate::timing::Span::new("home_confirmation_attempt");
            self.validate_owned_with_deadline(Some(until))
                .inspect_err(|_| {
                    crate::timing::outcome("home_owned_before", false);
                })?;
            let before = native_lock::query_until(&self.expected, until).state;
            crate::timing::outcome("home_native_before_unlocked", before == LockState::Unlocked);
            if before != LockState::Unlocked {
                return Err(AdapterError);
            }
            // WDA chooses the source application when /source starts. Wait for
            // Home before requesting the tree, so transition frames cannot pair
            // the old application's tree with SpringBoard metadata.
            let home_ready = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(wait_for_home_app(&UsbWda::new(self.usb.clone()), until));
            crate::timing::outcome("home_active_app_ready", home_ready.is_ok());
            home_ready?;
            let started = crate::timing::enabled().then(Instant::now);
            let mut timings = started.map(|_| CaptureTimings::default());
            let capture =
                self.runtime
                    .as_ref()
                    .expect("adapter runtime")
                    .block_on(before_input_deadline(until, async {
                        Self::capture(&self.usb, &self.expected, SCREEN_SOURCE, timings.as_mut())
                            .await
                            .map_err(Into::into)
                    }));
            if let (Some(started), Some(mut timings)) = (started, timings) {
                timings.finish(started, capture.is_ok());
                crate::timing::capture(timings);
            }
            let (raw, captured, reported) = capture?;
            let after = native_lock::query_until(&self.expected, proof_until).state;
            crate::timing::outcome("home_native_after_unlocked", after == LockState::Unlocked);
            self.validate_owned_with_deadline(Some(proof_until))
                .inspect_err(|_| {
                    crate::timing::outcome("home_owned_after", false);
                })?;
            let home = home_completion(&raw, before, after, reported, captured);
            crate::timing::outcome("home_evidence", home.is_ok());
            if let Ok(reached) = &home {
                crate::timing::outcome("home_reached", *reached);
            }
            Ok((raw, home?))
        });
        crate::timing::outcome("home_confirmation", result.is_ok());
        result
    }

    fn observe_for_action(
        &mut self,
        action: &Action,
    ) -> Result<(RawObservation, Option<ActionAdmission>), AdapterError> {
        self.finish_pending_effect(true)?;
        self.error_diagnostic = None;
        self.launch_admission = None;
        let owned_input = matches!(
            action,
            Action::Tap { .. }
                | Action::LaunchApp { .. }
                | Action::ClickButton { .. }
                | Action::DoubleTap { .. }
                | Action::LongPress { .. }
                | Action::Swipe { .. }
                | Action::Home
                | Action::Text { .. }
                | Action::TextCurrent { .. }
                | Action::KeyCurrent { .. }
        ) && self.owned_session.is_some()
            && self.owned_runner.is_some();
        if !matches!(action, Action::LaunchApp { .. }) && !owned_input {
            return self.observe().map(|observation| (observation, None));
        }
        let use_hint = owned_input && !matches!(action, Action::Home);
        if !use_hint {
            self.cache.clear();
        }
        let mut native_before = if use_hint {
            self.native_hint()
        } else {
            self.native_fresh(None)
        };
        let (mut observation, mut captured_at, mut reported_lock) =
            self.capture_for_action(use_hint).inspect_err(|_| {
                self.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture);
            })?;
        let native_after = if use_hint && normal_hint_context(&observation, reported_lock) {
            // This is the same preparation hint, not a new native observation.
            // execute() still requires a fresh native result before sending input.
            native_before
        } else {
            self.cache.clear();
            if use_hint {
                // Special contexts require the original fresh bracket. A hint
                // already copied into native_before cannot serve as that proof.
                let (before, capture, after) = fresh_native_bracket(
                    || self.native_fresh(None),
                    || self.capture_checked_with_lock_report(SCREEN_SOURCE),
                )
                .inspect_err(|_| {
                    self.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture);
                })?;
                native_before = before;
                (observation, captured_at, reported_lock) = capture;
                after
            } else {
                self.native_fresh(None)
            }
        };
        self.validate_owned().inspect_err(|_| {
            self.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture);
        })?;
        let admission = if let Action::Tap { at } = action {
            native_modal_tap_deadline(
                &observation,
                owned_input,
                native_before,
                native_after,
                reported_lock,
                *at,
                captured_at,
            )
            .map(|until| ActionAdmission::NativeModalTap { at: *at, until })
            .or_else(|| {
                visible_tap_deadline(
                    &observation,
                    owned_input,
                    native_before,
                    native_after,
                    reported_lock,
                    *at,
                    captured_at,
                )
                .map(|until| ActionAdmission::VisibleTap { at: *at, until })
            })
        } else if let Action::ClickButton { target } = action {
            (owned_input
                && native_before == LockState::Unlocked
                && native_after == LockState::Unlocked
                && reported_lock == Some(false)
                && observation.state.lock == LockState::Unlocked
                && observation.state.wda_ready
                && observation
                    .state
                    .foreground_app
                    .as_deref()
                    .is_some_and(|app| app != "com.apple.springboard")
                && crate::wda_ui_admission::named_button(&observation.accessibility_tree, target))
            .then(|| captured_at.checked_add(TEXT_INPUT_AGE))
            .flatten()
            .map(|until| ActionAdmission::NativeButton { until })
        } else if matches!(
            action,
            Action::KeyCurrent { .. }
                | Action::Text { .. }
                | Action::TextCurrent { .. }
                | Action::DoubleTap { .. }
                | Action::LongPress { .. }
        ) {
            native_text_admission(
                &observation,
                owned_input,
                native_before,
                native_after,
                reported_lock,
                captured_at,
                action,
            )
        } else if let Action::Swipe {
            from,
            to,
            duration_ms,
        } = action
        {
            native_swipe_deadline(
                &observation,
                owned_input,
                native_before,
                native_after,
                reported_lock,
                action,
                captured_at,
            )
            .map(|until| ActionAdmission::NativeSwipe {
                from: *from,
                to: *to,
                duration_ms: *duration_ms,
                until,
            })
        } else if matches!(action, Action::Home) {
            native_home_deadline(
                &observation,
                owned_input,
                native_before,
                native_after,
                reported_lock,
                captured_at,
            )
            .map(|until| ActionAdmission::NativeHome { until })
        } else if let Action::LaunchApp { bundle_id } = action {
            if let Some(until) = native_app_launch_deadline(
                &observation,
                owned_input,
                native_before,
                native_after,
                reported_lock,
                captured_at,
                bundle_id,
            ) {
                self.launch_admission = Some((bundle_id.clone(), until));
                Some(ActionAdmission::NativeAppLaunch { until })
            } else {
                springboard_home_launch_deadline(
                    &observation,
                    native_before,
                    native_after,
                    reported_lock,
                    captured_at,
                )
                .map(|until| ActionAdmission::HomeLaunch { until })
            }
        } else {
            None
        };
        if matches!(action, Action::DoubleTap { .. } | Action::LongPress { .. })
            && admission.is_none()
        {
            eprintln!(
                "handkited: gesture_admission_missing native_before_unlocked={} native_after_unlocked={} reported_unlocked={} owned_session={} owned_runner={}",
                native_before == LockState::Unlocked,
                native_after == LockState::Unlocked,
                reported_lock == Some(false),
                self.owned_session.is_some(),
                self.owned_runner.is_some(),
            );
        }
        Ok((observation, admission))
    }

    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_motion(action, send_deadline, None)
    }

    fn native_lock_state(&mut self) -> LockState {
        native_lock::query(&self.expected).state
    }

    fn execute_with_admission(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        admission: Option<ActionAdmission>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_with_motion(action, send_deadline, admission, None)
    }

    fn execute_with_motion(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        admission: Option<ActionAdmission>,
        motion: Option<Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.finish_pending_effect(true)?;
        self.error_diagnostic = None;
        self.cache.clear_full();
        if self.owned_session.is_some()
            && matches!(action, Action::Text { .. } | Action::TextCurrent { .. })
            && !matches!(admission, Some(ActionAdmission::NativeText { .. }))
        {
            self.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture);
            return Err(AdapterError);
        }
        if let Some(ActionAdmission::NativeAppLaunch { until }) = admission {
            if !take_launch_admission(
                &mut self.launch_admission,
                action,
                until,
                send_deadline,
                Instant::now(),
            ) || self.owned_session.is_none()
                || self.owned_runner.is_none()
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if let Some(ActionAdmission::NativeSwipe {
            from,
            to,
            duration_ms,
            until,
        }) = admission
        {
            if !matches!(action, Action::Swipe { from: actual_from, to: actual_to, duration_ms: actual_duration }
                if *actual_from == from && *actual_to == to && *actual_duration == duration_ms)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if let Some(ActionAdmission::NativeKey { key, until }) = admission {
            if !matches!(action, Action::KeyCurrent { key: actual } if *actual == key)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if matches!(action, Action::KeyCurrent { .. }) {
            return Err(AdapterError);
        }
        if let Action::ClickButton { target } = action {
            let Some(ActionAdmission::NativeButton { until }) = admission else {
                self.error_diagnostic = Some(AdapterDiagnostic::ButtonAdmission);
                return Err(AdapterError);
            };
            if self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                self.error_diagnostic = Some(AdapterDiagnostic::ButtonAdmission);
                return Err(AdapterError);
            }
            self.validate_owned_with_deadline(Some(send_deadline))
                .inspect_err(|_| {
                    self.error_diagnostic = Some(AdapterDiagnostic::OwnedBefore);
                })?;
            let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
            let button = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(before_input_deadline(send_deadline, async {
                    crate::wda_button::resolve(
                        &wda,
                        self.actions.prepared_session().ok_or(AdapterError)?,
                        target,
                    )
                    .await
                }))
                .inspect_err(|_| {
                    self.error_diagnostic = Some(AdapterDiagnostic::ButtonResolve);
                })?;
            self.validate_owned_with_deadline(Some(send_deadline))
                .inspect_err(|_| {
                    self.error_diagnostic = Some(AdapterDiagnostic::OwnedBefore);
                })?;
            if self.native_fresh(Some(send_deadline)) != LockState::Unlocked
                || Instant::now() >= send_deadline
            {
                self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
                return Err(AdapterError);
            }
            let receipt = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(button.click(&wda))
                .inspect_err(|_| {
                    self.cache.clear();
                    self.error_diagnostic = Some(AdapterDiagnostic::ButtonClick);
                })?;
            self.validate_owned().inspect_err(|_| {
                self.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
            })?;
            return Ok(receipt);
        }
        if matches!(admission, Some(ActionAdmission::NativeButton { .. })) {
            return Err(AdapterError);
        }
        if let Some(ActionAdmission::NativeText { until }) = admission {
            if !matches!(action, Action::Text { .. } | Action::TextCurrent { .. })
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if let Some(ActionAdmission::NativeTextGesture { at, until }) = admission {
            if !matches!(action, Action::DoubleTap { at: actual } | Action::LongPress { at: actual, .. } if *actual == at)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if let Some(ActionAdmission::NativeModalTap { at, until }) = admission {
            if !matches!(action, Action::Tap { at: actual } if *actual == at)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            return self.execute_motion(action, send_deadline, motion);
        }
        if let Some(ActionAdmission::NativeHome { until }) = admission {
            if !matches!(action, Action::Home)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            self.validate_owned_with_deadline(Some(send_deadline))?;
            if native_lock::query_until(&self.expected, send_deadline).state != LockState::Unlocked
            {
                return Err(AdapterError);
            }
            let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
            let receipt = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(self.actions.activate_home_with_existing_session(&wda))
                .inspect_err(|_| self.cache.clear())?;
            return Ok(receipt);
        }
        self.execute_motion(action, send_deadline, motion)
    }
}

impl Drop for WdaAdapter {
    fn drop(&mut self) {
        // Teardown sends no new proof request; it only drains claimed work.
        let _ = self.abandon_pending_effect();
        let runtime = self.runtime.take().expect("adapter runtime");
        if let Some(runner) = self.owned_runner.take() {
            let wda = runner.write_admission().map(|admission| {
                UsbWda::new(self.usb.clone()).with_write_admission(Some(admission))
            });
            if let Some(lease) = self.owned_idle.take() {
                let restored = match wda.as_ref() {
                    Some(wda) if !runner.is_finished() => {
                        runtime.block_on(restore_owned_idle_settings(wda, lease))
                            == Restoration::Verified
                    }
                    _ => false,
                };
                if !restored {
                    eprintln!("handkite: owned idle settings restoration unproven");
                }
            }
            if let Some(session) = self.owned_session.take() {
                let closed = match wda.as_ref() {
                    Some(wda) if !runner.is_finished() => {
                        runtime.block_on(close_owned_session(wda, &session))
                            == SessionClose::Acknowledged
                    }
                    _ => false,
                };
                if !closed {
                    eprintln!("handkite: owned session close unproven");
                }
            }
            if runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), runner.abort_and_join()).await
            }) != Ok(Ok(()))
            {
                eprintln!("handkite: owned runner cleanup unproven");
            }
        }
        runtime.shutdown_timeout(Duration::from_secs(5));
    }
}

fn response_value(response: HttpResponse) -> Result<serde_json::Value, AdapterError> {
    if response.status != 200 {
        return Err(AdapterError);
    }
    let mut json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    json.get_mut("value")
        .map(serde_json::Value::take)
        .ok_or(AdapterError)
}

fn unix_ms() -> Result<u64, AdapterError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AdapterError)?
        .as_millis() as u64)
}

fn classify_lock(app: &str, tree: &str, reported: Option<bool>) -> LockState {
    if ["proud-lock", "passcode-view", "call-to-action-label"]
        .iter()
        .any(|marker| tree.contains(marker))
    {
        return LockState::Locked;
    }
    if app != "com.apple.springboard" && reported == Some(false) {
        return LockState::Unlocked;
    }
    LockState::Uncertain
}

fn native_modal_tap_deadline(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    at: Point,
    captured_at: Instant,
) -> Option<Instant> {
    if !owned
        || native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Uncertain
        || !observation.state.wda_ready
        || observation.state.foreground_app.as_deref() != Some("com.apple.springboard")
        || !crate::wda_ui_admission::visible_modal_button(&observation.accessibility_tree, at)
    {
        return None;
    }
    captured_at.checked_add(HOME_LAUNCH_AGE)
}

fn visible_tap_deadline(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    at: Point,
    captured_at: Instant,
) -> Option<Instant> {
    if !owned
        || native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Unlocked
        || !observation.state.wda_ready
        || observation
            .state
            .foreground_app
            .as_deref()
            .is_none_or(|app| app == "com.apple.springboard")
    {
        return None;
    }
    let age = if crate::wda_ui_admission::visible_button(&observation.accessibility_tree, at) {
        HOME_LAUNCH_AGE
    } else if crate::wda_ui_admission::visible_text_target(&observation.accessibility_tree, at)
        || crate::wda_ui_admission::visible_text_focus_target(&observation.accessibility_tree, at)
    {
        TEXT_INPUT_AGE
    } else {
        return None;
    };
    captured_at.checked_add(age)
}

fn native_text_admission(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    captured_at: Instant,
    action: &Action,
) -> Option<ActionAdmission> {
    if !owned
        || native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Unlocked
        || !observation.state.wda_ready
        || observation
            .state
            .foreground_app
            .as_deref()
            .is_none_or(|app| app == "com.apple.springboard")
    {
        return None;
    }
    let until = captured_at.checked_add(TEXT_INPUT_AGE)?;
    let xml = &observation.accessibility_tree;
    match action {
        Action::Text { .. } if crate::wda_ui_admission::visible_text_input(xml) => {
            Some(ActionAdmission::NativeText { until })
        }
        Action::TextCurrent { .. } if crate::wda_ui_admission::visible_current_text_input(xml) => {
            Some(ActionAdmission::NativeText { until })
        }
        Action::KeyCurrent { key } if crate::wda_ui_admission::visible_current_text_input(xml) => {
            Some(ActionAdmission::NativeKey { key: *key, until })
        }
        Action::DoubleTap { at } | Action::LongPress { at, .. }
            if crate::wda_ui_admission::visible_text_target(xml, *at) =>
        {
            Some(ActionAdmission::NativeTextGesture { at: *at, until })
        }
        _ => None,
    }
}

fn native_swipe_deadline(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    action: &Action,
    captured_at: Instant,
) -> Option<Instant> {
    let Action::Swipe {
        from,
        to,
        duration_ms,
    } = action
    else {
        return None;
    };
    if !owned
        || native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Unlocked
        || !observation.state.wda_ready
        || observation
            .state
            .foreground_app
            .as_deref()
            .is_none_or(|app| app == "com.apple.springboard")
        || !(1..=5000).contains(duration_ms)
        || !crate::wda_ui_admission::swipe_context(&observation.accessibility_tree, *from, *to)
    {
        return None;
    }
    captured_at.checked_add(TEXT_INPUT_AGE)
}

fn normal_hint_context(observation: &RawObservation, reported: Option<bool>) -> bool {
    reported == Some(false)
        && observation.state.lock == LockState::Unlocked
        && observation
            .state
            .foreground_app
            .as_deref()
            .is_some_and(|app| app != "com.apple.springboard")
        && crate::wda_ui_admission::safe_context(&observation.accessibility_tree)
}

type CheckedCapture = (RawObservation, Instant, Option<bool>);

fn fresh_native_bracket(
    mut native: impl FnMut() -> LockState,
    capture: impl FnOnce() -> Result<CheckedCapture, AdapterError>,
) -> Result<(LockState, CheckedCapture, LockState), AdapterError> {
    let before = native();
    let capture = capture()?;
    let after = native();
    Ok((before, capture, after))
}

fn native_app_launch_deadline(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    captured_at: Instant,
    bundle_id: &str,
) -> Option<Instant> {
    if bundle_id.is_empty() || bundle_id.len() > 256 {
        return None;
    }
    // App launches use the same unlocked, nonmodal context as Home, with
    // the shorter text-input freshness window and a one-use target binding.
    native_home_deadline(
        observation,
        owned,
        native_before,
        native_after,
        reported_lock,
        captured_at,
    )?;
    captured_at.checked_add(TEXT_INPUT_AGE)
}

fn take_launch_admission(
    binding: &mut Option<(String, Instant)>,
    action: &Action,
    until: Instant,
    send_deadline: Instant,
    now: Instant,
) -> bool {
    let target = binding.take();
    matches!((action, target),
        (Action::LaunchApp { bundle_id }, Some((expected, deadline)))
            if bundle_id == &expected && until == deadline)
        && now < until
        && send_deadline <= until
}

fn native_home_deadline(
    observation: &RawObservation,
    owned: bool,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    captured_at: Instant,
) -> Option<Instant> {
    if !owned
        || native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Unlocked
        || !observation.state.wda_ready
        || observation
            .state
            .foreground_app
            .as_deref()
            .is_none_or(|app| app == "com.apple.springboard")
        || !crate::wda_ui_admission::safe_context(&observation.accessibility_tree)
    {
        return None;
    }
    captured_at.checked_add(HOME_LAUNCH_AGE)
}

fn wait_for_home(
    until: Instant,
    proof_until: Instant,
    now: &impl Fn() -> Instant,
    mut capture: impl FnMut(Instant, Instant) -> Result<(RawObservation, bool), AdapterError>,
) -> Result<RawObservation, AdapterError> {
    loop {
        if now() >= until {
            return Err(AdapterError);
        }
        let (observation, home) = capture(until, proof_until)?;
        if now() >= proof_until {
            return Err(AdapterError);
        }
        if home {
            return Ok(observation);
        }
    }
}

async fn wait_for_home_app(wda: &impl WdaRequest, until: Instant) -> Result<(), AdapterError> {
    let _timing = crate::timing::Span::new("home_active_app_wait");
    before_input_deadline(until, async {
        loop {
            let response = wda
                .request("GET", "/wda/activeAppInfo", b"", Duration::from_secs(5))
                .await
                .map_err(|_| AdapterError)?;
            let value = response_value(response)?;
            match value["bundleId"].as_str().filter(|app| !app.is_empty()) {
                Some("com.apple.springboard") => return Ok(()),
                Some(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                None => return Err(AdapterError),
            }
        }
    })
    .await
}

fn home_completion(
    observation: &RawObservation,
    before: LockState,
    after: LockState,
    reported: Option<bool>,
    captured: Instant,
) -> Result<bool, AdapterError> {
    if springboard_home_launch_deadline(observation, before, after, reported, captured).is_some() {
        return Ok(true);
    }
    // Only a valid, still-unlocked app capture is a transition worth observing again.
    if native_home_deadline(observation, true, before, after, reported, captured).is_some() {
        return Ok(false);
    }
    Err(AdapterError)
}

/// One-capture private admission for launching an app from unlocked Home.
/// It never changes the public lock classification or authorizes direct input.
pub fn springboard_home_launch_deadline(
    observation: &RawObservation,
    native_before: LockState,
    native_after: LockState,
    reported_lock: Option<bool>,
    captured_at: Instant,
) -> Option<Instant> {
    let tree = &observation.accessibility_tree;
    if native_before != LockState::Unlocked
        || native_after != LockState::Unlocked
        || reported_lock != Some(false)
        || observation.state.lock != LockState::Uncertain
        || observation.state.foreground_app.as_deref() != Some("com.apple.springboard")
        || !tree.contains("XCUIElementTypeApplication")
        || !tree.contains("XCUIElementTypeIcon")
        || [
            "proud-lock",
            "passcode-view",
            "call-to-action-label",
            "Enter Passcode",
            "XCUIElementTypeAlert",
            "XCUIElementTypeSheet",
            "XCUIElementTypeSecureTextField",
        ]
        .iter()
        .any(|marker| tree.contains(marker))
    {
        return None;
    }
    captured_at.checked_add(HOME_LAUNCH_AGE)
}

impl WdaAdapter {
    /// One send. `motion` reshapes only transport motion; every check is unchanged.
    fn execute_motion(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        motion: Option<Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.finish_pending_effect(true)?;
        self.error_diagnostic = None;
        self.validate_owned_with_deadline(Some(send_deadline))
            .inspect_err(|_| {
                self.error_diagnostic = Some(AdapterDiagnostic::OwnedBefore);
            })?;
        if (matches!(action, Action::LaunchApp { .. })
            || matches!(
                action,
                Action::Tap { .. }
                    | Action::DoubleTap { .. }
                    | Action::LongPress { .. }
                    | Action::Swipe { .. }
                    | Action::Text { .. }
                    | Action::TextCurrent { .. }
                    | Action::Key { .. }
                    | Action::KeyCurrent { .. }
            ) && self.owned_session.is_some())
            && self.native_fresh(Some(send_deadline)) != LockState::Unlocked
        {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
        let receipt = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(
                self.actions
                    .execute_with_existing_session_diagnostic(&wda, action, motion),
            )
            .map_err(|diagnostic| {
                self.cache.clear();
                self.error_diagnostic = Some(diagnostic);
                AdapterError
            })?;
        self.validate_owned().inspect_err(|_| {
            self.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
        })?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_startup_error_has_a_fixed_owner_action() {
        let error = OwnedStartupError::Runner(RunnerError::PairingRequired);
        assert_eq!(error.category(), "pairing_required");
        assert_eq!(
            error.message(),
            "pairing_required: owner must accept trust/passcode on device"
        );
        let other = OwnedStartupError::Runner(RunnerError::AutomationInitializationFailed);
        assert_eq!(other.message(), other.category());
    }

    #[test]
    fn authorization_startup_error_has_a_fixed_owner_action() {
        let error = OwnedStartupError::Runner(RunnerError::AuthorizationRequired);
        assert_eq!(error.category(), "authorization_required");
        assert_eq!(
            error.message(),
            "authorization_required: owner must complete XCTest Enable UI Automation on device"
        );
        assert_eq!(
            RunnerError::AuthorizationRequired.to_string(),
            error.message()
        );
    }

    #[test]
    fn hinted_app_to_home_or_modal_requires_a_new_native_bracket() {
        for home in [true, false] {
            let mut raw = home_sample(home);
            if !home {
                raw.accessibility_tree = "<XCUIElementTypeAlert/>".into();
            }
            assert!(!normal_hint_context(&raw, Some(false)));
            let calls = std::cell::RefCell::new(Vec::new());
            let (before, _, after) = fresh_native_bracket(
                || {
                    calls.borrow_mut().push("native");
                    LockState::Locked
                },
                || {
                    calls.borrow_mut().push("capture");
                    Ok((raw, Instant::now(), Some(false)))
                },
            )
            .unwrap();
            assert_eq!(*calls.borrow(), ["native", "capture", "native"]);
            assert_eq!((before, after), (LockState::Locked, LockState::Locked));
        }
    }

    #[test]
    fn app_launch_admission_requires_unlocked_owned_safe_context() {
        let captured = Instant::now();
        for change in 0..15 {
            let mut observation = home_sample(false);
            observation.accessibility_tree =
                r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"/></AppiumAUT>"#.into();
            let (mut owned, mut before, mut after, mut reported, mut bundle) = (
                true,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false),
                "com.example.notes".to_string(),
            );
            match change {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => reported = None,
                5 => reported = Some(true),
                6 => observation.state.lock = LockState::Locked,
                7 => observation.state.wda_ready = false,
                8 => observation.state.foreground_app = None,
                9 => observation.state.foreground_app = Some("com.apple.springboard".into()),
                10 => observation.accessibility_tree = "<broken>".into(),
                11 => observation.accessibility_tree = "<XCUIElementTypeAlert/>".into(),
                12 => bundle.clear(),
                13 => bundle = "a".repeat(257),
                14 => observation.accessibility_tree = "<XCUIElementTypeApplication/>".into(),
                _ => {}
            }
            assert_eq!(
                native_app_launch_deadline(
                    &observation,
                    owned,
                    before,
                    after,
                    reported,
                    captured,
                    &bundle
                ),
                (change == 0).then_some(captured + TEXT_INPUT_AGE),
                "case {change}"
            );
        }
    }

    #[test]
    fn app_launch_binding_is_exact_and_consumed_even_on_refusal() {
        let now = Instant::now();
        let until = now + TEXT_INPUT_AGE;
        for change in 0..6 {
            let mut binding = Some(("com.example.notes".into(), until));
            let mut action = Action::LaunchApp {
                bundle_id: "com.example.notes".into(),
            };
            let (mut admission_until, mut deadline, mut at) = (until, until, now);
            match change {
                1 => {
                    action = Action::LaunchApp {
                        bundle_id: "com.example.other".into(),
                    }
                }
                2 => action = Action::Home,
                3 => admission_until += Duration::from_millis(1),
                4 => deadline += Duration::from_millis(1),
                5 => at = until,
                _ => {}
            }
            assert_eq!(
                take_launch_admission(&mut binding, &action, admission_until, deadline, at),
                change == 0
            );
            assert!(binding.is_none());
            assert!(!take_launch_admission(
                &mut binding,
                &action,
                until,
                until,
                now
            ));
        }
    }

    #[test]
    fn early_admission_failure_does_not_reuse_previous_diagnostic() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture-phone").unwrap()).unwrap();
        adapter.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
        assert_eq!(
            adapter.execute_with_admission(
                &Action::KeyCurrent {
                    key: handkite_core::protocol::KeyboardKey::Backspace
                },
                Instant::now() + Duration::from_secs(1),
                None,
            ),
            Err(AdapterError)
        );
        assert_eq!(adapter.take_error_diagnostic(), None);
    }

    #[test]
    fn native_swipe_admission_requires_a_safe_context_and_both_native_checks() {
        let captured = Instant::now();
        let from = Point { x: 195.0, y: 650.0 };
        let to = Point { x: 195.0, y: 250.0 };
        for change in 0..13 {
            let mut observation = home_sample(false);
            observation.accessibility_tree = r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"/></AppiumAUT>"#.into();
            let mut owned = true;
            let mut before = LockState::Unlocked;
            let mut after = LockState::Unlocked;
            let mut reported = Some(false);
            let mut action = Action::Swipe {
                from,
                to,
                duration_ms: 500,
            };
            match change {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => reported = None,
                5 => reported = Some(true),
                6 => observation.state.lock = LockState::Locked,
                7 => observation.state.wda_ready = false,
                8 => observation.state.foreground_app = Some("com.apple.springboard".into()),
                9 => observation.state.foreground_app = None,
                10 => action = Action::Home,
                11 => {
                    action = Action::Swipe {
                        from,
                        to,
                        duration_ms: 0,
                    }
                }
                12 => observation.accessibility_tree = "<broken>".into(),
                _ => {}
            }
            let result = native_swipe_deadline(
                &observation,
                owned,
                before,
                after,
                reported,
                &action,
                captured,
            );
            assert_eq!(
                result,
                (change == 0).then_some(captured + TEXT_INPUT_AGE),
                "case {change}"
            );
        }
    }

    fn home_sample(home: bool) -> RawObservation {
        let id = HardwareId::new("fixture-device").unwrap();
        RawObservation {
            connected_usb: id.clone(),
            wda_target: id,
            window: ObservationWindow {
                started_unix_ms: 0,
                finished_unix_ms: 1,
            },
            state: DeviceState {
                lock: if home {
                    LockState::Uncertain
                } else {
                    LockState::Unlocked
                },
                wda_ready: true,
                foreground_app: Some(
                    if home {
                        "com.apple.springboard"
                    } else {
                        "com.example.clock"
                    }
                    .into(),
                ),
            },
            accessibility_tree: if home {
                "<XCUIElementTypeApplication><XCUIElementTypeIcon/></XCUIElementTypeApplication>"
            } else {
                r#"<XCUIElementTypeApplication x="0" y="0" width="393" height="852"/>"#
            }
            .into(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![],
            },
        }
    }

    #[test]
    fn home_completion_requires_native_unlock_and_home_evidence() {
        let now = Instant::now();
        for home in [false, true] {
            let mut sample = home_sample(home);
            assert_eq!(
                home_completion(
                    &sample,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false),
                    now
                ),
                Ok(home)
            );
            for (before, after, reported) in [
                (LockState::Locked, LockState::Unlocked, Some(false)),
                (LockState::Unlocked, LockState::Uncertain, Some(false)),
                (LockState::Unlocked, LockState::Unlocked, Some(true)),
                (LockState::Unlocked, LockState::Unlocked, None),
            ] {
                assert!(home_completion(&sample, before, after, reported, now).is_err());
            }
            sample
                .accessibility_tree
                .push_str("<XCUIElementTypeSecureTextField/>");
            assert!(
                home_completion(
                    &sample,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false),
                    now
                )
                .is_err()
            );
        }
    }

    #[test]
    fn home_completion_observes_transition_without_resetting_deadline() {
        let start = Instant::now();
        let until = start + Duration::from_millis(5);
        let elapsed = std::cell::Cell::new(0);
        let mut captures = 0;
        let result = wait_for_home(
            until,
            until,
            &|| start + Duration::from_millis(elapsed.get()),
            |deadline, proof_deadline| {
                assert_eq!(proof_deadline, until);
                assert_eq!(deadline, until);
                captures += 1;
                elapsed.set(elapsed.get() + 1);
                Ok((home_sample(captures == 2), captures == 2))
            },
        )
        .unwrap();
        assert_eq!(captures, 2);
        assert_eq!(
            result.state.foreground_app.as_deref(),
            Some("com.apple.springboard")
        );

        elapsed.set(0);
        captures = 0;
        assert!(
            wait_for_home(
                until,
                until,
                &|| start + Duration::from_millis(elapsed.get()),
                |deadline, proof_deadline| {
                    assert_eq!(proof_deadline, until);
                    assert_eq!(deadline, until);
                    captures += 1;
                    elapsed.set(elapsed.get() + 1);
                    Ok((home_sample(false), false))
                }
            )
            .is_err()
        );
        assert_eq!(captures, 5);
    }

    #[test]
    fn home_completion_stops_on_capture_failure_or_late_home() {
        let start = Instant::now();
        let until = start + Duration::from_millis(5);
        let now = std::cell::Cell::new(start);
        let mut captures = 0;
        assert!(
            wait_for_home(until, until, &|| now.get(), |_, _| {
                captures += 1;
                Err(AdapterError)
            })
            .is_err()
        );
        assert_eq!(captures, 1);
        assert!(
            wait_for_home(until, until, &|| now.get(), |_, _| {
                now.set(until);
                Ok((home_sample(true), true))
            })
            .is_err()
        );
        assert!(
            wait_for_home(until, until, &|| now.get(), |_, _| panic!(
                "expired capture polled"
            ))
            .is_err()
        );
    }

    #[test]
    fn home_proof_tail_cannot_start_another_capture() {
        let start = Instant::now();
        let acquisition = start + HOME_COMPLETION_BUDGET;
        let proof = acquisition + HOME_PROOF_TAIL;
        let now = std::cell::Cell::new(start);
        for (reached, finished, accepted) in [
            (true, acquisition + Duration::from_millis(500), true),
            (true, proof, false),
            (false, acquisition, false),
        ] {
            now.set(start);
            let mut captures = 0;
            let result =
                wait_for_home(acquisition, proof, &|| now.get(), |capture_by, prove_by| {
                    assert_eq!(capture_by, acquisition);
                    assert_eq!(prove_by, proof);
                    captures += 1;
                    now.set(finished);
                    Ok((home_sample(reached), reached))
                });
            assert_eq!(result.is_ok(), accepted);
            assert_eq!(captures, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn home_app_wait_is_read_only_and_stops_on_invalid_evidence() {
        struct Apps(std::cell::RefCell<std::collections::VecDeque<serde_json::Value>>);
        impl WdaRequest for Apps {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _deadline: Duration,
            ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
                assert_eq!(
                    (method, path, body),
                    ("GET", "/wda/activeAppInfo", b"".as_slice())
                );
                let value = self.0.borrow_mut().pop_front().expect("unexpected retry");
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&serde_json::json!({"value": value})).unwrap(),
                })
            }
        }
        for (apps, accepted) in [
            (
                vec![
                    serde_json::json!({"bundleId":"com.example.notes"}),
                    serde_json::json!({"bundleId":"com.apple.springboard"}),
                ],
                true,
            ),
            (vec![serde_json::json!({"bundleId":""})], false),
            (vec![serde_json::json!({"error":"unknown error"})], false),
        ] {
            let wda = Apps(std::cell::RefCell::new(apps.into()));
            assert_eq!(
                wait_for_home_app(&wda, Instant::now() + Duration::from_secs(1))
                    .await
                    .is_ok(),
                accepted
            );
            assert!(wda.0.borrow().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn home_app_wait_bounds_stalled_reads_and_skips_expired_reads() {
        struct Pending(std::cell::Cell<usize>);
        impl WdaRequest for Pending {
            async fn request(
                &self,
                _: &str,
                _: &str,
                _: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
                self.0.set(self.0.get() + 1);
                std::future::pending().await
            }
        }
        let wda = Pending(std::cell::Cell::new(0));
        assert!(wait_for_home_app(&wda, Instant::now()).await.is_err());
        assert_eq!(wda.0.get(), 0);
        let start = tokio::time::Instant::now();
        assert!(
            wait_for_home_app(&wda, Instant::now() + Duration::from_millis(100))
                .await
                .is_err()
        );
        assert_eq!(wda.0.get(), 1);
        assert!(start.elapsed() <= Duration::from_millis(101));
    }

    #[tokio::test(start_paused = true)]
    async fn home_acquisition_allows_the_measured_snapshot_tail() {
        // Native and active-app checks plus the full Home snapshot crossed
        // five seconds on device. The remaining proof still has its own cap.
        let started = tokio::time::Instant::now();
        let result = before_input_deadline(Instant::now() + HOME_COMPLETION_BUDGET, async {
            tokio::time::sleep(Duration::from_millis(5_050)).await;
            Ok(())
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(started.elapsed(), Duration::from_millis(5_050));
    }

    #[tokio::test]
    async fn expired_input_validation_does_not_poll_work() {
        let polled = std::cell::Cell::new(false);
        let result = before_input_deadline(Instant::now(), async {
            polled.set(true);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!polled.get());
    }

    #[tokio::test(start_paused = true)]
    async fn input_validation_stops_at_remaining_budget() {
        let started = tokio::time::Instant::now();
        let result: Result<(), AdapterError> = before_input_deadline(
            Instant::now() + Duration::from_millis(100),
            std::future::pending(),
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() <= Duration::from_millis(101));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_element_lookup_stops_before_click() {
        struct PendingLookup(std::cell::Cell<usize>);
        impl WdaRequest for PendingLookup {
            async fn request(
                &self,
                method: &str,
                path: &str,
                _body: &[u8],
                _deadline: Duration,
            ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
                assert_eq!(method, "POST");
                assert_eq!(path, "/session/fixture/elements");
                self.0.set(self.0.get() + 1);
                std::future::pending().await
            }
        }
        let wda = PendingLookup(std::cell::Cell::new(0));
        let clicked = std::cell::Cell::new(false);
        let target = handkite_core::protocol::ButtonTarget {
            label: "Close".into(),
            x: 1.0,
            y: 1.0,
            width: 30.0,
            height: 20.0,
        };
        let result = before_input_deadline(Instant::now() + Duration::from_millis(100), async {
            let button = crate::wda_button::resolve(&wda, "fixture", &target).await?;
            clicked.set(true);
            button.click(&wda).await
        })
        .await;
        assert!(result.is_err());
        assert_eq!(wda.0.get(), 1);
        assert!(!clicked.get());
    }

    #[test]
    fn response_values_preserve_null_and_reject_missing_or_failed_envelopes() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"nested": [1, 2]}),
            serde_json::json!("screen"),
        ] {
            let body = serde_json::to_vec(&serde_json::json!({"value": value})).unwrap();
            assert_eq!(
                response_value(HttpResponse { status: 200, body }).unwrap(),
                value
            );
        }
        for (status, body) in [
            (200, b"{}".as_slice()),
            (200, b"null"),
            (200, b"invalid"),
            (500, b"{\"value\":null}"),
        ] {
            assert!(
                response_value(HttpResponse {
                    status,
                    body: body.to_vec()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn borrowed_adapter_has_no_runner_liveness() {
        let adapter = WdaAdapter::new(HardwareId::new("fixture-device").unwrap()).unwrap();
        assert!(adapter.runner_liveness().is_none());
    }

    #[test]
    fn owned_text_without_native_admission_stops_before_transport() {
        for action in [
            Action::Text {
                value: "fixture".into(),
            },
            Action::TextCurrent {
                value: "fixture".into(),
            },
        ] {
            let mut adapter = WdaAdapter::new(HardwareId::new("fixture-device").unwrap()).unwrap();
            adapter.owned_session = Some("fixture-session".into());
            let runtime = adapter.runtime.take();
            let result = adapter.execute_with_admission(
                &action,
                Instant::now() + Duration::from_secs(1),
                None,
            );
            let diagnostic = adapter.take_error_diagnostic();
            adapter.runtime = runtime;
            assert!(result.is_err());
            assert_eq!(diagnostic, Some(AdapterDiagnostic::PreActionCapture));
        }
    }

    #[test]
    fn text_admission_keeps_the_capture_deadline_and_native_checks() {
        let id = HardwareId::new("fixture-device").unwrap();
        let mut observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Unlocked, wda_ready: true,
                foreground_app: Some("com.example.search".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeSearchField visible="true" enabled="true" x="20" y="67" width="298" height="44"/><XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let original = observation.state.clone();
        let captured = Instant::now() - Duration::from_secs(6);
        for change in 0..10 {
            observation.state = original.clone();
            let mut owned = true;
            let mut before = LockState::Unlocked;
            let mut after = LockState::Unlocked;
            let mut raw = Some(false);
            match change {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => raw = None,
                5 => raw = Some(true),
                6 => observation.state.lock = LockState::Locked,
                7 => observation.state.wda_ready = false,
                8 => observation.state.foreground_app = Some("com.apple.springboard".into()),
                9 => observation.state.foreground_app = None,
                _ => {}
            }
            let action = Action::Text {
                value: "fixture".into(),
            };
            let result =
                native_text_admission(&observation, owned, before, after, raw, captured, &action);
            if change == 0 {
                let until = captured + Duration::from_secs(5);
                assert_eq!(result, Some(ActionAdmission::NativeText { until }));
                assert!(until < Instant::now());
            } else {
                assert_eq!(result, None, "case {change}");
            }
        }
    }

    #[test]
    fn text_view_admission_is_scoped_to_current_keys_and_point_gestures() {
        use handkite_core::protocol::KeyboardKey;
        let id = HardwareId::new("fixture-device").unwrap();
        let observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Unlocked, wda_ready: true,
                foreground_app: Some("com.example.notes".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="544"/><XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let captured = Instant::now();
        let until = captured + Duration::from_secs(5);
        let at = Point { x: 80.0, y: 40.0 };
        for (action, expected) in [
            (
                Action::KeyCurrent {
                    key: KeyboardKey::Enter,
                },
                Some(ActionAdmission::NativeKey {
                    key: KeyboardKey::Enter,
                    until,
                }),
            ),
            (
                Action::KeyCurrent {
                    key: KeyboardKey::Backspace,
                },
                Some(ActionAdmission::NativeKey {
                    key: KeyboardKey::Backspace,
                    until,
                }),
            ),
            (
                Action::DoubleTap { at },
                Some(ActionAdmission::NativeTextGesture { at, until }),
            ),
            (
                Action::LongPress {
                    at,
                    duration_ms: 800,
                },
                Some(ActionAdmission::NativeTextGesture { at, until }),
            ),
            (
                Action::Key {
                    key: KeyboardKey::Enter,
                },
                None,
            ),
            (
                Action::Text {
                    value: "fixture".into(),
                },
                None,
            ),
            (
                Action::TextCurrent {
                    value: "fixture".into(),
                },
                Some(ActionAdmission::NativeText { until }),
            ),
            (
                Action::LongPress {
                    at: Point { x: 80.0, y: 580.0 },
                    duration_ms: 800,
                },
                None,
            ),
        ] {
            assert_eq!(
                native_text_admission(
                    &observation,
                    true,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false),
                    captured,
                    &action
                ),
                expected
            );
        }
    }

    #[test]
    fn native_modal_tap_requires_owned_unlocked_springboard_and_keeps_home_refused() {
        let id = HardwareId::new("fixture-device").unwrap();
        let mut observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Uncertain, wda_ready: true,
                foreground_app: Some("com.apple.springboard".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeAlert visible="true" enabled="true" x="30" y="100" width="330" height="650"><XCUIElementTypeButton label="Decline" visible="true" enabled="true" x="50" y="680" width="290" height="50"/></XCUIElementTypeAlert></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let at = Point { x: 195.0, y: 705.0 };
        let captured = Instant::now();
        for change in 0..10 {
            let mut owned = true;
            let mut before = LockState::Unlocked;
            let mut after = LockState::Unlocked;
            let mut raw = Some(false);
            match change {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => raw = None,
                5 => raw = Some(true),
                6 => observation.state.lock = LockState::Locked,
                7 => observation.state.wda_ready = false,
                8 => observation.state.foreground_app = Some("com.example.app".into()),
                9 => observation.accessibility_tree = "<root/>".into(),
                _ => {}
            }
            let result =
                native_modal_tap_deadline(&observation, owned, before, after, raw, at, captured);
            if change == 0 {
                assert_eq!(result, Some(captured + Duration::from_secs(2)));
                assert_eq!(
                    native_home_deadline(&observation, owned, before, after, raw, captured),
                    None
                );
            } else {
                assert_eq!(result, None, "case {change}");
            }
            observation.state.lock = LockState::Uncertain;
            observation.state.wda_ready = true;
            observation.state.foreground_app = Some("com.apple.springboard".into());
        }
    }

    #[test]
    fn editor_tap_uses_text_deadline_with_native_unlocked_context() {
        let id = HardwareId::new("fixture-device").unwrap();
        let mut observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Unlocked, wda_ready: true,
                foreground_app: Some("com.example.notes".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="544"/><XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let at = Point { x: 80.0, y: 40.0 };
        let captured = Instant::now();
        for case in 0..8 {
            let mut owned = true;
            let mut before = LockState::Unlocked;
            let mut after = LockState::Unlocked;
            let mut reported = Some(false);
            let mut point = at;
            match case {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => reported = None,
                5 => point.y = 550.0,
                6 => observation.state.lock = LockState::Uncertain,
                7 => observation.state.wda_ready = false,
                _ => {}
            }
            let result = visible_tap_deadline(
                &observation,
                owned,
                before,
                after,
                reported,
                point,
                captured,
            );
            assert_eq!(
                result,
                (case == 0).then_some(captured + TEXT_INPUT_AGE),
                "case {case}"
            );
            observation.state.lock = LockState::Unlocked;
            observation.state.wda_ready = true;
        }
    }

    #[test]
    fn reading_view_tap_keeps_native_guards_without_enabling_text_input() {
        let id = HardwareId::new("fixture-device").unwrap();
        let mut observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Unlocked, wda_ready: true,
                foreground_app: Some("com.example.notes".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="852"/></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let at = Point { x: 55.0, y: 136.0 };
        let captured = Instant::now();
        for case in 0..8 {
            let mut owned = true;
            let mut before = LockState::Unlocked;
            let mut after = LockState::Unlocked;
            let mut reported = Some(false);
            match case {
                1 => owned = false,
                2 => before = LockState::Locked,
                3 => after = LockState::Uncertain,
                4 => reported = Some(true),
                5 => reported = None,
                6 => observation.state.foreground_app = Some("com.apple.springboard".into()),
                7 => observation.state.lock = LockState::Uncertain,
                _ => {}
            }
            assert_eq!(
                visible_tap_deadline(&observation, owned, before, after, reported, at, captured),
                (case == 0).then_some(captured + TEXT_INPUT_AGE)
            );
            for action in [
                Action::TextCurrent { value: "x".into() },
                Action::KeyCurrent {
                    key: handkite_core::protocol::KeyboardKey::Backspace,
                },
                Action::LongPress {
                    at,
                    duration_ms: 500,
                },
            ] {
                assert_eq!(
                    native_text_admission(
                        &observation,
                        owned,
                        before,
                        after,
                        reported,
                        captured,
                        &action
                    ),
                    None
                );
            }
            observation.state.foreground_app = Some("com.example.notes".into());
            observation.state.lock = LockState::Unlocked;
        }
    }

    #[test]
    fn input_admission_requires_owned_native_unlocked_context_and_capture_deadline() {
        let id = HardwareId::new("fixture-device").unwrap();
        let mut observation = RawObservation {
            connected_usb: id.clone(), wda_target: id,
            window: ObservationWindow { started_unix_ms: 0, finished_unix_ms: 1 },
            state: DeviceState { lock: LockState::Unlocked, wda_ready: true, foreground_app: Some("com.example.app".into()) },
            accessibility_tree: r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852"><XCUIElementTypeButton label="Clear" visible="true" enabled="true" x="270" y="490" width="30" height="30"/></XCUIElementTypeApplication></AppiumAUT>"#.into(),
            screenshot: ScreenImage { format: ImageFormat::Png, bytes: vec![] },
        };
        let at = Point { x: 285.0, y: 505.0 };
        let captured_at = Instant::now();
        let deadline = |observation: &RawObservation, owned, before, after, raw| {
            let tap = visible_tap_deadline(observation, owned, before, after, raw, at, captured_at);
            let home = native_home_deadline(observation, owned, before, after, raw, captured_at);
            assert_eq!(tap, home);
            home
        };
        assert_eq!(
            deadline(
                &observation,
                true,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false)
            ),
            Some(captured_at + Duration::from_secs(2))
        );
        assert_eq!(
            deadline(
                &observation,
                false,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false)
            ),
            None
        );
        for lock in [LockState::Locked, LockState::Uncertain] {
            assert_eq!(
                deadline(&observation, true, lock, LockState::Unlocked, Some(false)),
                None
            );
            assert_eq!(
                deadline(&observation, true, LockState::Unlocked, lock, Some(false)),
                None
            );
            observation.state.lock = lock;
            assert_eq!(
                deadline(
                    &observation,
                    true,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false)
                ),
                None
            );
            observation.state.lock = LockState::Unlocked;
        }
        for raw in [None, Some(true)] {
            assert_eq!(
                deadline(
                    &observation,
                    true,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    raw
                ),
                None
            );
        }
        observation.state.wda_ready = false;
        assert_eq!(
            deadline(
                &observation,
                true,
                LockState::Unlocked,
                LockState::Unlocked,
                Some(false)
            ),
            None
        );
        observation.state.wda_ready = true;
        for app in [None, Some("com.apple.springboard".into())] {
            observation.state.foreground_app = app;
            assert_eq!(
                deadline(
                    &observation,
                    true,
                    LockState::Unlocked,
                    LockState::Unlocked,
                    Some(false)
                ),
                None
            );
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_runner_startup() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct Mark(Arc<AtomicBool>);
        impl Drop for Mark {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.store(true, Ordering::Release);
        });
        let work = async {
            let _guard = Mark(dropped.clone());
            std::future::pending::<Result<(), AdapterError>>().await
        };
        assert_eq!(
            run_until_shutdown(work, &|| stop.load(Ordering::Acquire), AdapterError).await,
            Err(AdapterError)
        );
        setter.await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn preexisting_shutdown_refuses_owned_startup_before_device_access() {
        let expected = HardwareId::new("fixture-device").unwrap();
        assert_eq!(
            WdaAdapter::new_owned(expected, "com.example.xctrunner", || true, || true).err(),
            Some(OwnedStartupError::ShutdownRequested)
        );
    }

    #[test]
    fn owned_startup_preserves_actionable_fixed_categories() {
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::AutomationWaitTimedOut).category(),
            "automation_wait_timed_out"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::AutomationInitializationFailed).category(),
            "automation_initialization_failed"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::InitializationFailed).category(),
            "runner_initialization_failed"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::EndpointBusy).category(),
            "runner_endpoint_busy"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::NativeProcess(
                crate::wda_runner::NativeProcessStep::Preexisting
            ))
            .category(),
            "process_preexisting"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::PlanStartUnproven).category(),
            "runner_plan_start_unproven"
        );
    }

    #[test]
    fn visible_lock_overrides_a_stale_wda_flag() {
        assert_eq!(
            classify_lock(
                "com.apple.springboard",
                "<node name=\"proud-lock\" />",
                Some(false)
            ),
            LockState::Locked
        );
        assert_eq!(
            classify_lock("com.apple.springboard", "<home />", Some(false)),
            LockState::Uncertain
        );
        assert_eq!(
            classify_lock("com.example.app", "<app />", Some(false)),
            LockState::Unlocked
        );
    }
}
