use crate::native_lock;
use crate::pin_field;
use crate::pin_screen::{self, ParsedPasscodeView};
use crate::usb::PinnedUsb;
use crate::wda_action::{WdaActions, WdaRequest};
use crate::wda_transport::{UsbWda, WdaError};
use handkite_core::controller::AdapterError;
use handkite_core::passcode::PasscodeProgress;
use handkite_core::protocol::{Action, HardwareId, LockState, Point, TransportReceipt};
use handkite_core::unlock::{BackendError, PasscodeBackend, PasscodeScreen};
use std::thread;
use std::time::{Duration, Instant};

const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_VIEW_BUDGET: Duration = Duration::from_secs(5);
const SUPERVISED_WAKE_SETTLE: Duration = Duration::from_millis(1500);

struct PreparedPinView {
    view: ParsedPasscodeView,
    fresh_until: Instant,
    empty_locked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ProgressCheck {
    Disabled,
    BeforeFirst,
    Pending,
    Complete,
    Failed,
}

pub struct NativePasscodeBackend {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
    last_view: Option<PreparedPinView>,
    last_diagnostics: Option<PinDiagnostics>,
    supervised_visibility_fallback: bool,
    session_bound: bool,
    awaiting_first_digit: bool,
    progress_check: ProgressCheck,
    coordinate_pin_taps: bool,
}

#[derive(Clone, Copy)]
pub struct PinDiagnostics {
    pub snapshot_ms: u128,
    pub screen_size: bool,
    pub field_seen: bool,
    pub field_accepted: bool,
    pub keys_seen: usize,
    pub keys_eligible: usize,
    pub keys_visible: usize,
    pub keys_enabled: usize,
    pub field_visible: bool,
    pub field_enabled: bool,
    pub layout_valid: bool,
    pub visibility_fallback: bool,
}

impl NativePasscodeBackend {
    pub fn new(expected: HardwareId) -> Result<Self, BackendError> {
        Self::new_with_supervised_visibility_fallback(expected, false)
    }

    pub fn new_with_supervised_visibility_fallback(
        expected: HardwareId,
        supervised_visibility_fallback: bool,
    ) -> Result<Self, BackendError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| BackendError::Unavailable)?;
        Ok(Self {
            usb: PinnedUsb::new(expected.clone()),
            expected,
            runtime,
            actions: WdaActions::default(),
            last_view: None,
            last_diagnostics: None,
            supervised_visibility_fallback,
            session_bound: false,
            awaiting_first_digit: false,
            progress_check: ProgressCheck::Disabled,
            coordinate_pin_taps: false,
        })
    }

    /// Opt-in backend for a settings transaction; every input revalidates its session.
    pub fn new_for_session(
        expected: HardwareId,
        supervised_visibility_fallback: bool,
        session: &str,
    ) -> Result<Self, BackendError> {
        let actions =
            WdaActions::bound_to_existing(session).map_err(|_| BackendError::Unavailable)?;
        let mut backend = Self::new_with_supervised_visibility_fallback(
            expected,
            supervised_visibility_fallback,
        )?;
        backend.actions = actions;
        backend.session_bound = true;
        backend.progress_check = ProgressCheck::BeforeFirst;
        Ok(backend)
    }

    pub fn diagnostics(&self) -> Option<PinDiagnostics> {
        self.last_diagnostics
    }

    /// Coordinate PIN taps are available only inside the supervised session bracket.
    pub fn with_coordinate_pin_taps(mut self) -> Result<Self, BackendError> {
        if !self.session_bound || !self.supervised_visibility_fallback {
            return Err(BackendError::Unavailable);
        }
        self.coordinate_pin_taps = true;
        Ok(self)
    }

    fn request_action(&mut self, action: &Action) -> Result<(), BackendError> {
        self.request_action_with(action, Instant::now(), |backend, action, fresh_until| {
            let wda = UsbWda::new(backend.usb.clone()).with_write_deadline(fresh_until);
            backend
                .runtime
                .block_on(async {
                    if backend.session_bound {
                        bound_action(
                            &mut backend.actions,
                            &wda,
                            action,
                            backend.coordinate_pin_taps,
                        )
                        .await
                    } else {
                        backend.actions.execute_existing(&wda, action).await
                    }
                })
                .map(|_| ())
                .map_err(|_| BackendError::Unavailable)
        })
    }

    fn request_action_with(
        &mut self,
        action: &Action,
        now: Instant,
        dispatch: impl FnOnce(&mut Self, &Action, Instant) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        let prepared = self.last_view.take().ok_or(BackendError::Unavailable)?;
        if now >= prepared.fresh_until
            || matches!(
                self.progress_check,
                ProgressCheck::Pending | ProgressCheck::Failed
            )
        {
            return Err(BackendError::Unavailable);
        }
        dispatch(self, action, prepared.fresh_until)
    }

    fn press_digit_with(
        &mut self,
        digit: u8,
        dispatch: impl FnOnce(&mut Self, &Action) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        if matches!(
            self.progress_check,
            ProgressCheck::Pending | ProgressCheck::Failed
        ) {
            return Err(BackendError::Unavailable);
        }
        let at = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.keypad.as_ref())
            .and_then(|keys| keys.get(usize::from(digit)))
            .copied()
            .ok_or(BackendError::Unavailable)?;
        self.awaiting_first_digit = false;
        let result = dispatch(self, &Action::Tap { at });
        if self.progress_check == ProgressCheck::BeforeFirst {
            self.progress_check = ProgressCheck::Pending;
        }
        result
    }

    fn wake_hidden_with(
        &mut self,
        now: Instant,
        wake: impl FnOnce(&mut Self, Instant) -> Result<(), BackendError>,
        settle: impl FnOnce(Duration),
        snapshot: impl FnOnce(&mut Self) -> Result<PasscodeScreen, BackendError>,
    ) -> Result<(), BackendError> {
        let prepared = self.last_view.take().ok_or(BackendError::Unavailable)?;
        if !self.supervised_visibility_fallback
            || !self.session_bound
            || now >= prepared.fresh_until
            || !prepared.empty_locked
            || !prepared.view.visibility_fallback
        {
            return Err(BackendError::Unavailable);
        }
        wake(self, prepared.fresh_until)?;
        settle(SUPERVISED_WAKE_SETTLE);
        // The core samples again before marking the gate. Keep the same zero-progress
        // requirement through that snapshot, until the first digit dispatch begins.
        self.awaiting_first_digit = true;
        snapshot(self)?;
        if !self
            .last_view
            .as_ref()
            .is_some_and(|view| view.empty_locked)
        {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        Ok(())
    }
}

async fn bound_action(
    actions: &mut WdaActions,
    wda: &impl WdaRequest,
    action: &Action,
    coordinate_pin_taps: bool,
) -> Result<TransportReceipt, AdapterError> {
    if coordinate_pin_taps {
        let Action::Tap { at } = action else {
            return Err(AdapterError);
        };
        actions.tap_with_existing_session(wda, *at).await
    } else {
        actions.execute_with_existing_session(wda, action).await
    }
}

fn empty_locked(screen: &PasscodeScreen, view: &ParsedPasscodeView, expected: &HardwareId) -> bool {
    screen.connected_usb == *expected
        && screen.wda_target == *expected
        && screen.native_lock == LockState::Locked
        && screen.visible_lock == LockState::Locked
        && screen.wda_lock == LockState::Locked
        && !screen.before_first_unlock
        && !screen.trust_prompt
        && screen.keypad_ready
        && !view.has_alert
        && view.visible_lock == LockState::Locked
        && view.layout_valid
        && view.keypad.is_some()
        && [&screen.secure_field_value, &view.secure_field_value]
            .into_iter()
            .all(|field| {
                field
                    .as_deref()
                    .and_then(|value| PasscodeProgress::parse(value, 6).ok())
                    .is_some_and(|progress| progress.entered == 0)
            })
}

fn compare_progress(
    screen: &PasscodeScreen,
    view: &ParsedPasscodeView,
    direct: u32,
) -> Result<(), BackendError> {
    if screen.native_lock != LockState::Locked
        || screen.visible_lock != LockState::Locked
        || screen.wda_lock != LockState::Locked
        || screen.before_first_unlock
        || screen.trust_prompt
        || view.has_alert
        || !screen.keypad_ready
        || !view.layout_valid
        || view.keypad.is_none()
    {
        return Err(BackendError::Unavailable);
    }
    let source = pin_field::count(
        view.secure_field_value
            .as_deref()
            .ok_or(BackendError::Unavailable)?,
    )
    .map_err(|_| BackendError::Unavailable)?;
    if source != direct {
        return Err(BackendError::ProgressMismatch);
    }
    Ok(())
}

fn wda_error(error: WdaError) -> BackendError {
    match error {
        WdaError::Usb(_) => BackendError::Disconnected,
        _ => BackendError::Unavailable,
    }
}

fn response_value(
    response: crate::wda_http::HttpResponse,
) -> Result<serde_json::Value, BackendError> {
    if response.status != 200 {
        return Err(BackendError::Unavailable);
    }
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| BackendError::Unavailable)?;
    json.get("value").cloned().ok_or(BackendError::Unavailable)
}

fn keypad_swipe(size: (f64, f64)) -> Result<Action, BackendError> {
    let (width, height) = size;
    if !width.is_finite()
        || !height.is_finite()
        || !(200.0..=2000.0).contains(&width)
        || !(400.0..=2000.0).contains(&height)
    {
        return Err(BackendError::Unavailable);
    }
    Ok(Action::Swipe {
        from: Point {
            x: width / 2.0,
            y: height - 8.0,
        },
        to: Point {
            x: width / 2.0,
            y: height * 0.45,
        },
        duration_ms: 300,
    })
}

impl NativePasscodeBackend {
    fn snapshot_with(
        &mut self,
        started: Instant,
        capture: impl FnOnce(
            &mut Self,
            Instant,
        ) -> Result<(PasscodeScreen, ParsedPasscodeView), BackendError>,
        now: impl FnOnce() -> Instant,
    ) -> Result<PasscodeScreen, BackendError> {
        self.last_view = None;
        if self.progress_check == ProgressCheck::Failed {
            return Err(BackendError::Unavailable);
        }
        let comparing = self.progress_check == ProgressCheck::Pending;
        let fresh_until = started
            .checked_add(PIN_VIEW_BUDGET)
            .ok_or(BackendError::Unavailable)?;
        let captured = capture(self, started);
        if comparing {
            self.progress_check = ProgressCheck::Failed;
        }
        let (screen, view) = captured?;
        if now() >= fresh_until {
            return Err(BackendError::Unavailable);
        }
        let empty_locked = empty_locked(&screen, &view, &self.expected);
        if self.awaiting_first_digit && !empty_locked {
            return Err(BackendError::Unavailable);
        }
        self.last_view = Some(PreparedPinView {
            view,
            fresh_until,
            empty_locked,
        });
        if comparing {
            self.progress_check = ProgressCheck::Complete;
        }
        Ok(screen)
    }

    fn capture_snapshot(
        &mut self,
        started: Instant,
    ) -> Result<(PasscodeScreen, ParsedPasscodeView), BackendError> {
        let wda = UsbWda::new(self.usb.clone());
        self.runtime
            .block_on(self.actions.prepare_existing(&wda))
            .map_err(|_| BackendError::Unavailable)?;
        let (source, reported_lock) = self.runtime.block_on(async {
            tokio::join!(
                wda.request("GET", PIN_SOURCE, b"", Duration::from_secs(10)),
                wda.request("GET", "/wda/locked", b"", Duration::from_secs(10)),
            )
        });
        let source = response_value(source.map_err(wda_error)?)?;
        let reported_lock = response_value(reported_lock.map_err(wda_error)?)?;
        let direct = if self.progress_check == ProgressCheck::Pending {
            let session = self
                .actions
                .prepared_session()
                .ok_or(BackendError::Unavailable)?;
            Some(
                self.runtime
                    .block_on(pin_field::read(&wda, session))
                    .map_err(|_| BackendError::Unavailable)?,
            )
        } else {
            None
        };
        if self.session_bound {
            self.runtime
                .block_on(self.actions.prepare_existing(&wda))
                .map_err(|_| BackendError::Unavailable)?;
        }
        let xml = source.as_str().ok_or(BackendError::Unavailable)?;
        let parsed = pin_screen::parse(xml).map_err(|_| BackendError::Unavailable)?;
        let native = native_lock::query(&self.expected);
        self.last_diagnostics = Some(PinDiagnostics {
            snapshot_ms: started.elapsed().as_millis(),
            screen_size: parsed.screen_size.is_some(),
            field_seen: parsed.field_seen,
            field_accepted: parsed.secure_field_value.is_some(),
            keys_seen: parsed.digit_keys_seen,
            keys_eligible: parsed.digit_keys_eligible,
            keys_visible: parsed.digit_keys_visible,
            keys_enabled: parsed.digit_keys_enabled,
            field_visible: parsed.field_visible,
            field_enabled: parsed.field_enabled,
            layout_valid: parsed.layout_valid,
            visibility_fallback: parsed.visibility_fallback,
        });
        let wda_lock = match reported_lock.as_bool() {
            Some(true) => LockState::Locked,
            Some(false) => LockState::Unlocked,
            None => LockState::Uncertain,
        };
        let fallback_blocked = parsed.visibility_fallback && !self.supervised_visibility_fallback;
        let screen = PasscodeScreen {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            native_lock: native.state,
            visible_lock: parsed.visible_lock,
            wda_lock,
            keypad_ready: !fallback_blocked
                && parsed.keypad.is_some()
                && parsed.secure_field_value.is_some(),
            secure_field_value: (!fallback_blocked)
                .then(|| parsed.secure_field_value.clone())
                .flatten(),
            before_first_unlock: native.unlocked_since_boot != Some(true),
            trust_prompt: parsed.has_alert,
        };
        if let Some(direct) = direct {
            compare_progress(&screen, &parsed, direct)?;
        }
        Ok((screen, parsed))
    }
}

impl PasscodeBackend for NativePasscodeBackend {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
        self.snapshot_with(Instant::now(), Self::capture_snapshot, Instant::now)
    }

    fn show_keypad(&mut self) -> Result<(), BackendError> {
        if matches!(
            self.progress_check,
            ProgressCheck::Pending | ProgressCheck::Failed
        ) {
            return Err(BackendError::Unavailable);
        }
        if self.supervised_visibility_fallback && self.session_bound {
            if self.last_view.as_ref().is_some_and(|prepared| {
                prepared.empty_locked
                    && !prepared.view.visibility_fallback
                    && prepared.view.field_visible
                    && prepared.view.digit_keys_visible == 10
            }) {
                return Ok(());
            }
            return self.wake_hidden_with(
                Instant::now(),
                |backend, fresh_until| {
                    let wda = UsbWda::new(backend.usb.clone()).with_write_deadline(fresh_until);
                    backend
                        .runtime
                        .block_on(backend.actions.wake_with_existing_session(&wda))
                        .map_err(|_| BackendError::Unavailable)
                },
                thread::sleep,
                Self::snapshot,
            );
        }
        if self.supervised_visibility_fallback {
            return self
                .last_view
                .as_ref()
                .filter(|prepared| {
                    prepared.view.visibility_fallback && prepared.view.keypad.is_some()
                })
                .map(|_| ())
                .ok_or(BackendError::Unavailable);
        }
        if self
            .last_view
            .as_ref()
            .is_some_and(|prepared| prepared.view.keypad.is_some())
        {
            return Ok(());
        }
        let prepared = self.last_view.take();
        let wda = UsbWda::new(self.usb.clone());
        if self.session_bound {
            let deadline = prepared.ok_or(BackendError::Unavailable)?.fresh_until;
            let wda = wda.with_write_deadline(deadline);
            self.runtime
                .block_on(self.actions.wake_with_existing_session(&wda))
                .map_err(|_| BackendError::Unavailable)?;
        } else {
            self.runtime
                .block_on(self.actions.wake_hid_existing(&wda))
                .map_err(|_| BackendError::Unavailable)?;
        }
        thread::sleep(Duration::from_millis(300));
        let screen = self.snapshot()?;
        if screen.keypad_ready {
            return Ok(());
        }
        let size = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.screen_size)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&keypad_swipe(size)?)?;
        for _ in 0..3 {
            thread::sleep(Duration::from_millis(150));
            if self.snapshot()?.keypad_ready {
                return Ok(());
            }
        }
        Err(BackendError::Unavailable)
    }

    fn press_digit(&mut self, digit: u8) -> Result<(), BackendError> {
        self.press_digit_with(digit, Self::request_action)
    }

    fn delete_digit(&mut self) -> Result<(), BackendError> {
        if self.supervised_visibility_fallback {
            return Err(BackendError::Unavailable);
        }
        let at = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.delete_key)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&Action::Tap { at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> NativePasscodeBackend {
        NativePasscodeBackend::new(HardwareId::new("fixture-phone").unwrap()).unwrap()
    }

    fn capture() -> (PasscodeScreen, ParsedPasscodeView) {
        let id = HardwareId::new("fixture-phone").unwrap();
        (
            PasscodeScreen {
                connected_usb: id.clone(),
                wda_target: id,
                native_lock: LockState::Locked,
                visible_lock: LockState::Locked,
                wda_lock: LockState::Locked,
                keypad_ready: false,
                secure_field_value: None,
                before_first_unlock: false,
                trust_prompt: false,
            },
            pin_screen::parse("<XCUIElementTypeApplication width=\"400\" height=\"800\"/>")
                .unwrap(),
        )
    }

    fn prepare(backend: &mut NativePasscodeBackend, started: Instant) {
        backend
            .snapshot_with(started, |_, _| Ok(capture()), || started)
            .unwrap();
    }

    fn hidden_capture() -> (PasscodeScreen, ParsedPasscodeView) {
        keypad_capture(true)
    }

    fn keypad_capture(hidden: bool) -> (PasscodeScreen, ParsedPasscodeView) {
        let mut xml = String::from(
            r#"<XCUIElementTypeApplication width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..10 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="false" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + column * 100, 300 + row * 100));
        }
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="false" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/></XCUIElementTypeApplication>"#);
        let xml = if hidden {
            xml
        } else {
            xml.replace("visible=\"false\"", "visible=\"true\"")
        };
        let view = pin_screen::parse(&xml).unwrap();
        let (mut screen, _) = capture();
        screen.keypad_ready = true;
        screen.secure_field_value = view.secure_field_value.clone();
        (screen, view)
    }

    fn bound_backend() -> NativePasscodeBackend {
        NativePasscodeBackend::new_for_session(
            HardwareId::new("fixture-phone").unwrap(),
            true,
            "fixture-session",
        )
        .unwrap()
    }

    fn progress_capture(entered: u32) -> (PasscodeScreen, ParsedPasscodeView) {
        let (mut screen, mut view) = keypad_capture(false);
        let value = format!("{entered} of 6 digits");
        screen.secure_field_value = Some(value.clone());
        view.secure_field_value = Some(value);
        (screen, view)
    }

    #[test]
    fn coordinate_selection_requires_a_supervised_bound_backend() {
        assert!(backend().with_coordinate_pin_taps().is_err());
        assert!(
            NativePasscodeBackend::new_with_supervised_visibility_fallback(
                HardwareId::new("fixture-phone").unwrap(),
                true
            )
            .unwrap()
            .with_coordinate_pin_taps()
            .is_err()
        );
        assert!(
            NativePasscodeBackend::new_for_session(
                HardwareId::new("fixture-phone").unwrap(),
                false,
                "fixture-session"
            )
            .unwrap()
            .with_coordinate_pin_taps()
            .is_err()
        );
        assert!(!bound_backend().coordinate_pin_taps);
        assert!(
            bound_backend()
                .with_coordinate_pin_taps()
                .unwrap()
                .coordinate_pin_taps
        );
    }

    #[test]
    fn all_six_digit_taps_keep_the_selected_route_geometry_and_first_comparison() {
        use crate::wda_http::HttpResponse;
        use serde_json::{Value, json};
        use std::cell::Cell;
        struct Fake {
            coordinate: bool,
            calls: Cell<usize>,
            at: Cell<Point>,
        }
        impl WdaRequest for Fake {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, WdaError> {
                let call = self.calls.get();
                self.calls.set(call + 1);
                let value = match call % 3 {
                    0 => {
                        assert_eq!((method, path), ("GET", "/status"));
                        assert!(body.is_empty());
                        json!({"sessionId":"fixture-session","value":{"ready":true}})
                    }
                    1 => {
                        assert_eq!((method, path), ("GET", "/session/fixture-session"));
                        assert!(body.is_empty());
                        json!({"value":{}})
                    }
                    _ => {
                        let endpoint = if self.coordinate {
                            "/session/fixture-session/wda/tap"
                        } else {
                            "/session/fixture-session/actions"
                        };
                        assert_eq!((method, path), ("POST", endpoint));
                        let body: Value = serde_json::from_slice(body).unwrap();
                        let at = self.at.get();
                        if self.coordinate {
                            assert_eq!(body, json!({"x":at.x,"y":at.y}));
                        } else {
                            let movement = &body["actions"][0]["actions"][0];
                            assert_eq!(movement["x"], at.x);
                            assert_eq!(movement["y"], at.y);
                        }
                        json!({"value":null})
                    }
                };
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap(),
                })
            }
        }
        for coordinate in [false, true] {
            let mut backend = bound_backend();
            if coordinate {
                backend = backend.with_coordinate_pin_taps().unwrap();
            }
            let fake = Fake {
                coordinate,
                calls: Cell::new(0),
                at: Cell::new(Point { x: 0.0, y: 0.0 }),
            };
            let start = Instant::now();
            let mut comparisons = 0;
            for entered in 0..6 {
                backend
                    .snapshot_with(
                        start,
                        |backend, _| {
                            let (screen, view) = progress_capture(entered);
                            if backend.progress_check == ProgressCheck::Pending {
                                comparisons += 1;
                                compare_progress(&screen, &view, entered)?;
                            }
                            Ok((screen, view))
                        },
                        || start,
                    )
                    .unwrap();
                let digit = entered as u8;
                fake.at.set(
                    backend.last_view.as_ref().unwrap().view.keypad.unwrap()[usize::from(digit)],
                );
                backend
                    .press_digit_with(digit, |backend, action| {
                        backend.request_action_with(action, start, |backend, action, expires| {
                            assert_eq!(expires, start + PIN_VIEW_BUDGET);
                            backend
                                .runtime
                                .block_on(bound_action(
                                    &mut backend.actions,
                                    &fake,
                                    action,
                                    backend.coordinate_pin_taps,
                                ))
                                .map(|_| ())
                                .map_err(|_| BackendError::Unavailable)
                        })
                    })
                    .unwrap();
                assert!(backend.last_view.is_none());
            }
            assert_eq!(fake.calls.get(), 18);
            assert_eq!(comparisons, 1);
            assert_eq!(backend.progress_check, ProgressCheck::Complete);
            assert_eq!(backend.delete_digit(), Err(BackendError::Unavailable));
            assert_eq!(fake.calls.get(), 18);
        }
    }

    #[test]
    fn first_digit_comparison_runs_once_only_for_the_bound_backend() {
        for bound in [false, true] {
            let mut backend = if bound { bound_backend() } else { backend() };
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(progress_capture(0)), || start)
                .unwrap();
            let mut reads = 0;
            for entered in 1..=2 {
                backend
                    .press_digit_with(0, |backend, action| {
                        backend.request_action_with(action, start, |_, _, _| Ok(()))
                    })
                    .unwrap();
                backend
                    .snapshot_with(
                        start,
                        |backend, _| {
                            let (screen, view) = progress_capture(entered);
                            if backend.progress_check == ProgressCheck::Pending {
                                reads += 1;
                                compare_progress(&screen, &view, entered)?;
                            }
                            Ok((screen, view))
                        },
                        || start,
                    )
                    .unwrap();
            }
            assert_eq!(reads, usize::from(bound));
            assert_eq!(
                backend.progress_check,
                if bound {
                    ProgressCheck::Complete
                } else {
                    ProgressCheck::Disabled
                }
            );
        }
    }

    #[test]
    fn failed_comparison_consumes_the_snapshot_and_blocks_every_later_input() {
        for direct in [Ok(1), Err(BackendError::Unavailable)] {
            let mut backend = bound_backend();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(progress_capture(0)), || start)
                .unwrap();
            backend
                .press_digit_with(0, |backend, action| {
                    backend.request_action_with(action, start, |_, _, _| Ok(()))
                })
                .unwrap();
            let mut reads = 0;
            let result = backend.snapshot_with(
                start,
                |_, _| {
                    reads += 1;
                    let (screen, view) = progress_capture(0);
                    compare_progress(&screen, &view, direct?)?;
                    Ok((screen, view))
                },
                || start,
            );
            assert_eq!(
                result.err(),
                Some(if direct.is_ok() {
                    BackendError::ProgressMismatch
                } else {
                    BackendError::Unavailable
                })
            );
            assert_eq!(reads, 1);
            assert!(backend.last_view.is_none());
            assert_eq!(backend.progress_check, ProgressCheck::Failed);
            assert_eq!(
                backend.press_digit_with(0, |_, _| panic!("no next digit")),
                Err(BackendError::Unavailable)
            );
            assert!(
                backend
                    .snapshot_with(start, |_, _| panic!("no repeat comparison"), || start)
                    .is_err()
            );
            assert_eq!(backend.show_keypad(), Err(BackendError::Unavailable));
        }
    }

    #[test]
    fn comparison_failure_keeps_the_marked_gate_and_stops_before_digit_two() {
        use handkite_core::passcode::Passcode;
        use handkite_core::protocol::{DeviceBinding, DeviceKey};
        use handkite_core::unlock::{
            AttemptGate, GateError, UnlockError, UnlockPolicy, unlock_once,
        };
        struct Harness {
            backend: NativePasscodeBackend,
            start: Instant,
            taps: usize,
            reads: usize,
        }
        impl PasscodeBackend for Harness {
            fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
                let reads = &mut self.reads;
                self.backend.snapshot_with(
                    self.start,
                    |backend, _| {
                        let (screen, view) = progress_capture(0);
                        if backend.progress_check == ProgressCheck::Pending {
                            *reads += 1;
                            compare_progress(&screen, &view, 1)?;
                        }
                        Ok((screen, view))
                    },
                    || self.start,
                )
            }
            fn show_keypad(&mut self) -> Result<(), BackendError> {
                self.backend.show_keypad()
            }
            fn press_digit(&mut self, digit: u8) -> Result<(), BackendError> {
                let taps = &mut self.taps;
                let start = self.start;
                self.backend.press_digit_with(digit, |backend, action| {
                    backend.request_action_with(action, start, |_, _, _| {
                        *taps += 1;
                        Ok(())
                    })
                })
            }
            fn delete_digit(&mut self) -> Result<(), BackendError> {
                panic!("no deletion")
            }
        }
        #[derive(Default)]
        struct Gate {
            marked: bool,
        }
        impl AttemptGate for Gate {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(self.marked)
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                self.marked = true;
                Ok(())
            }
            fn clear(&mut self) -> Result<(), GateError> {
                panic!("mismatch cannot clear")
            }
        }
        for coordinate in [false, true] {
            let mut harness = Harness {
                backend: if coordinate {
                    bound_backend().with_coordinate_pin_taps().unwrap()
                } else {
                    bound_backend()
                },
                start: Instant::now(),
                taps: 0,
                reads: 0,
            };
            let mut gate = Gate::default();
            let binding = DeviceBinding {
                key: DeviceKey::new("iphone").unwrap(),
                usb_id: HardwareId::new("fixture-phone").unwrap(),
            };
            let code = Passcode::new("0".repeat(6)).unwrap();
            assert_eq!(
                unlock_once(
                    &mut harness,
                    &mut gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                ),
                Err(UnlockError::Backend(BackendError::ProgressMismatch))
            );
            assert_eq!((harness.taps, harness.reads), (1, 1));
            assert!(gate.marked);
            assert_eq!(
                unlock_once(
                    &mut harness,
                    &mut gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                ),
                Err(UnlockError::Disabled)
            );
            assert_eq!((harness.taps, harness.reads), (1, 1));
        }
    }

    #[test]
    fn visible_opt_in_and_existing_default_keypads_do_not_wake() {
        for (mut backend, hidden) in [
            (bound_backend(), false),
            (backend(), false),
            (
                NativePasscodeBackend::new_with_supervised_visibility_fallback(
                    HardwareId::new("fixture-phone").unwrap(),
                    true,
                )
                .unwrap(),
                true,
            ),
        ] {
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(keypad_capture(hidden)), || start)
                .unwrap();
            assert_eq!(backend.show_keypad(), Ok(()));
            assert!(!backend.awaiting_first_digit);
            assert!(backend.last_view.is_some());
        }
        let mut backend = backend();
        let start = Instant::now();
        let (mut screen, view) = keypad_capture(false);
        screen.secure_field_value = Some("1 of 6 digits".into());
        assert!(
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .is_ok()
        );
        assert_eq!(backend.show_keypad(), Ok(()));
    }

    #[test]
    fn supervised_bound_wake_consumes_original_deadline_and_settles_once() {
        let mut backend = bound_backend();
        let start = Instant::now();
        backend
            .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
            .unwrap();
        let mut wakes = 0;
        let mut settles = 0;
        let mut captures = 0;
        assert_eq!(
            backend.wake_hidden_with(
                start + Duration::from_millis(100),
                |backend, deadline| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(deadline, start + PIN_VIEW_BUDGET);
                    wakes += 1;
                    Ok(())
                },
                |delay| {
                    assert_eq!(delay, Duration::from_millis(1500));
                    settles += 1;
                },
                |backend| {
                    captures += 1;
                    backend.snapshot_with(
                        start + SUPERVISED_WAKE_SETTLE,
                        |_, _| Ok(hidden_capture()),
                        || start + SUPERVISED_WAKE_SETTLE,
                    )
                }
            ),
            Ok(())
        );
        assert_eq!((wakes, settles, captures), (1, 1, 1));
        assert!(backend.awaiting_first_digit);
        assert_eq!(
            backend.last_view.as_ref().unwrap().fresh_until,
            start + SUPERVISED_WAKE_SETTLE + PIN_VIEW_BUDGET
        );
    }

    #[test]
    fn hidden_wake_requires_opt_in_fresh_empty_locked_evidence() {
        type Change = fn(&mut PasscodeScreen, &mut ParsedPasscodeView);
        for change in [
            (|s: &mut PasscodeScreen, _: &mut ParsedPasscodeView| {
                s.wda_target = HardwareId::new("other").unwrap()
            }) as Change,
            |s, _| s.native_lock = LockState::Unlocked,
            |s, _| s.visible_lock = LockState::Uncertain,
            |s, _| s.wda_lock = LockState::Uncertain,
            |s, _| s.before_first_unlock = true,
            |s, _| s.trust_prompt = true,
            |s, _| s.keypad_ready = false,
            |s, _| s.secure_field_value = Some("1 of 6 digits".into()),
            |_, v| v.secure_field_value = Some("0 of 4 digits".into()),
            |_, v| v.has_alert = true,
            |_, v| v.visibility_fallback = false,
            |_, v| v.layout_valid = false,
        ] {
            let mut backend = bound_backend();
            let start = Instant::now();
            let (mut screen, mut view) = hidden_capture();
            change(&mut screen, &mut view);
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start,
                    |_, _| panic!("no wake"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
            assert!(backend.last_view.is_none());
        }
        for elapsed in [PIN_VIEW_BUDGET, PIN_VIEW_BUDGET + Duration::from_nanos(1)] {
            let mut backend = bound_backend();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start + elapsed,
                    |_, _| panic!("expired view"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
            assert!(backend.last_view.is_none());
        }
        for supervised in [false, true] {
            let mut backend = backend();
            backend.supervised_visibility_fallback = supervised;
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start,
                    |_, _| panic!("unbound path"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
        }
    }

    #[test]
    fn uncertain_wake_has_no_settle_reobserve_or_fallback() {
        let mut backend = bound_backend();
        let start = Instant::now();
        backend
            .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
            .unwrap();
        let mut wakes = 0;
        assert_eq!(
            backend.wake_hidden_with(
                start,
                |_, _| {
                    wakes += 1;
                    Err(BackendError::Unavailable)
                },
                |_| panic!("no settle"),
                |_| panic!("no capture")
            ),
            Err(BackendError::Unavailable)
        );
        assert_eq!(wakes, 1);
        assert!(backend.last_view.is_none());
        assert_eq!(
            backend.wake_hidden_with(
                start,
                |_, _| panic!("no replay"),
                |_| panic!("no settle"),
                |_| panic!("no capture")
            ),
            Err(BackendError::Unavailable)
        );
    }

    #[test]
    fn settled_and_core_pre_mark_snapshots_must_still_be_empty_and_locked() {
        use handkite_core::passcode::Passcode;
        use handkite_core::protocol::{DeviceBinding, DeviceKey};
        use handkite_core::unlock::{AttemptGate, GateError, UnlockPolicy, unlock_once};
        struct Harness {
            backend: NativePasscodeBackend,
            start: Instant,
            fail_settled: bool,
            samples: usize,
            wakes: usize,
        }
        impl PasscodeBackend for Harness {
            fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
                let (mut screen, view) = hidden_capture();
                if self.samples > 0 {
                    screen.wda_lock = LockState::Uncertain;
                }
                self.samples += 1;
                self.backend
                    .snapshot_with(self.start, |_, _| Ok((screen, view)), || self.start)
            }
            fn show_keypad(&mut self) -> Result<(), BackendError> {
                let fail = self.fail_settled;
                let start = self.start + SUPERVISED_WAKE_SETTLE;
                let wakes = &mut self.wakes;
                self.backend.wake_hidden_with(
                    self.start,
                    |_, _| {
                        *wakes += 1;
                        Ok(())
                    },
                    |_| {},
                    |backend| {
                        let (mut screen, view) = hidden_capture();
                        if fail {
                            screen.secure_field_value = Some("1 of 6 digits".into());
                        }
                        backend.snapshot_with(start, |_, _| Ok((screen, view)), || start)
                    },
                )
            }
            fn press_digit(&mut self, _: u8) -> Result<(), BackendError> {
                panic!("no digit submission")
            }
            fn delete_digit(&mut self) -> Result<(), BackendError> {
                panic!("no deletion")
            }
        }
        struct Gate;
        impl AttemptGate for Gate {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(false)
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                panic!("preflight must not mark")
            }
            fn clear(&mut self) -> Result<(), GateError> {
                panic!("preflight must not clear")
            }
        }
        for fail_settled in [false, true] {
            let mut harness = Harness {
                backend: bound_backend(),
                start: Instant::now(),
                fail_settled,
                samples: 0,
                wakes: 0,
            };
            let binding = DeviceBinding {
                key: DeviceKey::new("iphone").unwrap(),
                usb_id: HardwareId::new("fixture-phone").unwrap(),
            };
            // A synthetic code is required by the core interface but never read or submitted.
            let code = Passcode::new("0".repeat(6)).unwrap();
            assert!(
                unlock_once(
                    &mut harness,
                    &mut Gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                )
                .is_err()
            );
            assert_eq!(harness.wakes, 1);
            assert!(harness.backend.last_view.is_none());
        }
    }

    #[test]
    fn snapshot_deadline_starts_before_capture_and_rejects_expiry_at_equality() {
        for elapsed in [
            Duration::ZERO,
            PIN_VIEW_BUDGET - Duration::from_nanos(1),
            PIN_VIEW_BUDGET,
            PIN_VIEW_BUDGET + Duration::from_secs(1),
        ] {
            let mut backend = backend();
            let started = Instant::now();
            prepare(&mut backend, started);
            let result = backend.snapshot_with(
                started,
                |backend, capture_started| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(capture_started, started);
                    Ok(capture())
                },
                || started + elapsed,
            );
            if elapsed < PIN_VIEW_BUDGET {
                assert!(result.is_ok());
                let prepared = backend.last_view.as_ref().unwrap();
                assert_eq!(prepared.fresh_until, started + PIN_VIEW_BUDGET);
                assert_eq!(prepared.view.screen_size, Some((400.0, 800.0)));
            } else {
                assert!(matches!(result, Err(BackendError::Unavailable)));
                assert!(backend.last_view.is_none());
            }
        }
    }

    #[test]
    fn snapshot_failure_discards_prior_view_before_capture() {
        for error in [BackendError::Unavailable, BackendError::Disconnected] {
            let mut backend = backend();
            let started = Instant::now();
            prepare(&mut backend, started);
            let result = backend.snapshot_with(
                started,
                |backend, _| {
                    assert!(backend.last_view.is_none());
                    Err(error)
                },
                || panic!("failed capture cannot prepare a view"),
            );
            assert!(matches!(result, Err(actual) if actual == error));
            assert!(backend.last_view.is_none());
            assert_eq!(
                backend.request_action_with(&Action::Home, started, |_, _, _| {
                    panic!("failed capture cannot dispatch stale input")
                }),
                Err(BackendError::Unavailable)
            );
        }
    }

    #[test]
    fn dispatch_consumes_view_and_preserves_original_deadline_on_every_result() {
        for coordinate in [false, true] {
            for action in [
                Action::Tap {
                    at: Point { x: 80.0, y: 340.0 },
                },
                Action::Tap {
                    at: Point { x: 280.0, y: 640.0 },
                },
                keypad_swipe((400.0, 800.0)).unwrap(),
            ] {
                for result in [
                    Ok(()),
                    Err(BackendError::Unavailable),
                    Err(BackendError::Disconnected),
                ] {
                    let mut backend = if coordinate {
                        bound_backend().with_coordinate_pin_taps().unwrap()
                    } else {
                        backend()
                    };
                    let started = Instant::now();
                    backend
                        .snapshot_with(
                            started,
                            |_, _| Ok(capture()),
                            || started + Duration::from_secs(4),
                        )
                        .unwrap();
                    let mut dispatches = 0;
                    assert_eq!(
                        backend.request_action_with(
                            &action,
                            started + Duration::from_millis(4_500),
                            |backend, sent, fresh_until| {
                                assert!(backend.last_view.is_none());
                                assert!(std::ptr::eq(sent, &action));
                                assert_eq!(fresh_until, started + PIN_VIEW_BUDGET);
                                dispatches += 1;
                                result
                            },
                        ),
                        result
                    );
                    assert_eq!(dispatches, 1);
                    assert!(backend.last_view.is_none());
                    assert_eq!(
                        backend.request_action_with(&action, started, |_, _, _| {
                            panic!("consumed input cannot be retried")
                        }),
                        Err(BackendError::Unavailable)
                    );
                }
            }
        }
    }

    #[test]
    fn missing_or_expired_view_never_dispatches() {
        for coordinate in [false, true] {
            let action = Action::Tap {
                at: Point { x: 80.0, y: 340.0 },
            };
            let mut backend = if coordinate {
                bound_backend().with_coordinate_pin_taps().unwrap()
            } else {
                backend()
            };
            let started = Instant::now();
            assert_eq!(
                backend.request_action_with(&action, started, |_, _, _| panic!("missing view")),
                Err(BackendError::Unavailable)
            );
            for elapsed in [
                PIN_VIEW_BUDGET - Duration::from_nanos(1),
                PIN_VIEW_BUDGET,
                PIN_VIEW_BUDGET + Duration::from_nanos(1),
            ] {
                prepare(&mut backend, started);
                let mut dispatches = 0;
                let result = backend.request_action_with(&action, started + elapsed, |_, _, _| {
                    dispatches += 1;
                    Ok(())
                });
                if elapsed < PIN_VIEW_BUDGET {
                    assert_eq!(result, Ok(()));
                    assert_eq!(dispatches, 1);
                } else {
                    assert_eq!(result, Err(BackendError::Unavailable));
                    assert_eq!(dispatches, 0);
                }
                assert!(backend.last_view.is_none());
            }
        }
    }

    #[test]
    fn swipe_stays_inside_valid_phone_geometry() {
        let Action::Swipe {
            from,
            to,
            duration_ms,
        } = keypad_swipe((393.0, 852.0)).unwrap()
        else {
            panic!("expected swipe");
        };
        assert_eq!(from.x, 196.5);
        assert_eq!(from.y, 844.0);
        assert!(from.y > to.y);
        assert_eq!(duration_ms, 300);
        assert!(keypad_swipe((0.0, 852.0)).is_err());
    }
}
