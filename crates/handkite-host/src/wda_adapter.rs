use crate::native_lock;
use crate::usb::PinnedUsb;
use crate::wda_action::WdaActions;
use crate::wda_http::HttpResponse;
use crate::wda_runner::{OwnedWdaRunner, RunnerError, RunnerLiveness};
use crate::wda_session::{SessionClose, close_owned_session, create_owned_session};
use crate::wda_settings::validate_session;
use crate::wda_transport::UsbWda;
use base64::Engine;
use handkite_core::controller::{ActionAdmission, AdapterError, DeviceAdapter, RawObservation};
use handkite_core::protocol::{
    Action, DeviceState, HardwareId, ImageFormat, LockState, ObservationWindow, Point, ScreenImage,
    TransportReceipt,
};
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SCREEN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,index,traits,hittable,customActions,nativeFrame";
const ATTRIBUTE_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const HOME_LAUNCH_AGE: Duration = Duration::from_secs(2);

pub struct WdaAdapter {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: Option<tokio::runtime::Runtime>,
    actions: WdaActions,
    owned_runner: Option<OwnedWdaRunner>,
    owned_session: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedStartupError {
    ShutdownRequested,
    RuntimeUnavailable,
    Runner(RunnerError),
    SessionUnavailable,
}

impl OwnedStartupError {
    pub fn category(self) -> &'static str {
        match self {
            Self::ShutdownRequested => "shutdown_requested",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::Runner(RunnerError::AutomationAuthorizationRequired) => {
                "automation_authorization_required"
            }
            Self::Runner(_) => "runner_startup_failed",
            Self::SessionUnavailable => "session_unavailable",
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
            owned_runner: None,
            owned_session: None,
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
        Ok(adapter)
    }

    fn validate_owned(&self) -> Result<(), AdapterError> {
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
            .block_on(validate_session(&UsbWda::new(self.usb.clone()), session))
            .map_err(|_| AdapterError)?;
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
        self.validate_owned()?;
        let captured = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(Self::capture(&self.usb, &self.expected, source_path))?;
        self.validate_owned()?;
        Ok(captured)
    }

    async fn capture(
        usb: &PinnedUsb,
        expected: &HardwareId,
        source_path: &str,
    ) -> Result<(RawObservation, Instant, Option<bool>), AdapterError> {
        let started_unix_ms = unix_ms()?;
        let wda = UsbWda::new(usb.clone());
        let (image, source, app, reported_lock) = tokio::join!(
            wda.request("GET", "/screenshot", b"", Duration::from_secs(20)),
            wda.request("GET", source_path, b"", Duration::from_secs(20)),
            wda.request("GET", "/wda/activeAppInfo", b"", Duration::from_secs(10)),
            wda.request("GET", "/wda/locked", b"", Duration::from_secs(10)),
        );
        let image = response_value(image.map_err(|_| AdapterError)?)?;
        let source = response_value(source.map_err(|_| AdapterError)?)?;
        let app = response_value(app.map_err(|_| AdapterError)?)?;
        let reported_lock = response_value(reported_lock.map_err(|_| AdapterError)?)?;
        let encoded = image.as_str().ok_or(AdapterError)?;
        let screenshot = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| AdapterError)?;
        if !screenshot.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(AdapterError);
        }
        let tree = source
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or(AdapterError)?
            .to_owned();
        let app = app["bundleId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or(AdapterError)?
            .to_owned();
        let reported_lock = reported_lock.as_bool();
        let lock = classify_lock(&app, &tree, reported_lock);
        let finished = Instant::now();
        let observation = RawObservation {
            connected_usb: expected.clone(),
            wda_target: expected.clone(),
            window: ObservationWindow {
                started_unix_ms,
                finished_unix_ms: unix_ms()?,
            },
            state: DeviceState {
                lock,
                wda_ready: true,
                foreground_app: Some(app),
            },
            accessibility_tree: tree,
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: screenshot,
            },
        };
        Ok((observation, finished, reported_lock))
    }

    /// Diagnostic capture retaining visibility, enabled and selected attributes.
    /// The monotonic completion anchor bounds later diagnostic input freshness.
    /// Ordinary controller observations continue to use the smaller projection.
    pub fn observe_with_attributes(&mut self) -> Result<(RawObservation, Instant), AdapterError> {
        self.capture_checked(ATTRIBUTE_SOURCE)
    }

    /// Diagnostic-only raw lock reply paired with the same image and tree capture.
    /// Callers must combine it with fresh native state before authorizing input.
    pub fn observe_with_attributes_and_lock_report(
        &mut self,
    ) -> Result<(RawObservation, Instant, Option<bool>), AdapterError> {
        self.capture_checked_with_lock_report(ATTRIBUTE_SOURCE)
    }

    /// Paired diagnostic capture with the PIN parser's visible/enabled attributes.
    pub fn observe_pin_with_attributes(
        &mut self,
    ) -> Result<(RawObservation, Instant), AdapterError> {
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
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        self.validate_owned()?;
        self.runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(self.usb.provider())
            .map_err(|_| AdapterError)?;
        Ok((self.expected.clone(), self.expected.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.capture_checked(SCREEN_SOURCE)
            .map(|(observation, _)| observation)
    }

    fn observe_for_action(
        &mut self,
        action: &Action,
    ) -> Result<(RawObservation, Option<ActionAdmission>), AdapterError> {
        let owned_input = matches!(action, Action::Tap { .. } | Action::Home)
            && self.owned_session.is_some()
            && self.owned_runner.is_some();
        if !matches!(action, Action::LaunchApp { .. }) && !owned_input {
            return self.observe().map(|observation| (observation, None));
        }
        let native_before = native_lock::query(&self.expected).state;
        let (observation, captured_at, reported_lock) =
            self.capture_checked_with_lock_report(SCREEN_SOURCE)?;
        let native_after = native_lock::query(&self.expected).state;
        self.validate_owned()?;
        let admission = if let Action::Tap { at } = action {
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
        } else {
            springboard_home_launch_deadline(
                &observation,
                native_before,
                native_after,
                reported_lock,
                captured_at,
            )
            .map(|until| ActionAdmission::HomeLaunch { until })
        };
        Ok((observation, admission))
    }

    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.validate_owned()?;
        if (matches!(action, Action::LaunchApp { .. })
            || matches!(action, Action::Tap { .. }) && self.owned_session.is_some())
            && native_lock::query(&self.expected).state != LockState::Unlocked
        {
            return Err(AdapterError);
        }
        let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
        let receipt = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(self.actions.execute_with_existing_session(&wda, action))?;
        self.validate_owned()?;
        Ok(receipt)
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
        if let Some(ActionAdmission::NativeHome { until }) = admission {
            if !matches!(action, Action::Home)
                || self.owned_session.is_none()
                || self.owned_runner.is_none()
                || Instant::now() >= until
                || send_deadline > until
            {
                return Err(AdapterError);
            }
            self.validate_owned()?;
            if native_lock::query(&self.expected).state != LockState::Unlocked {
                return Err(AdapterError);
            }
            let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
            let receipt = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(self.actions.activate_home_with_existing_session(&wda))?;
            self.validate_owned()?;
            return Ok(receipt);
        }
        self.execute(action, send_deadline)
    }
}

impl Drop for WdaAdapter {
    fn drop(&mut self) {
        let runtime = self.runtime.take().expect("adapter runtime");
        if let Some(runner) = self.owned_runner.take() {
            if let Some(session) = self.owned_session.take() {
                if runner.is_finished() {
                    eprintln!("handkite: owned runner exited before session close");
                } else if runtime.block_on(close_owned_session(
                    &UsbWda::new(self.usb.clone()),
                    &session,
                )) != SessionClose::Acknowledged
                {
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
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    json.get("value").cloned().ok_or(AdapterError)
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
        || !crate::wda_ui_admission::visible_button(&observation.accessibility_tree, at)
    {
        return None;
    }
    captured_at.checked_add(HOME_LAUNCH_AGE)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_adapter_has_no_runner_liveness() {
        let adapter = WdaAdapter::new(HardwareId::new("fixture-device").unwrap()).unwrap();
        assert!(adapter.runner_liveness().is_none());
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
    fn owned_startup_keeps_automation_authorization_distinct() {
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::AutomationAuthorizationRequired).category(),
            "automation_authorization_required"
        );
        assert_eq!(
            OwnedStartupError::Runner(RunnerError::InitializationFailed).category(),
            "runner_startup_failed"
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
