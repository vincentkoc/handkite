use crate::native_lock;
use crate::pin_screen::{self, ParsedPasscodeView};
use crate::usb::PinnedUsb;
use crate::wda_action::WdaActions;
use crate::wda_transport::{UsbWda, WdaError};
use handkite_core::protocol::{Action, HardwareId, LockState, Point};
use handkite_core::unlock::{BackendError, PasscodeBackend, PasscodeScreen};
use std::thread;
use std::time::{Duration, Instant};

const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_VIEW_BUDGET: Duration = Duration::from_secs(5);

struct PreparedPinView {
    view: ParsedPasscodeView,
    fresh_until: Instant,
}

pub struct NativePasscodeBackend {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
    last_view: Option<PreparedPinView>,
    last_diagnostics: Option<PinDiagnostics>,
    supervised_visibility_fallback: bool,
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
        })
    }

    pub fn diagnostics(&self) -> Option<PinDiagnostics> {
        self.last_diagnostics
    }

    fn request_action(&mut self, action: &Action) -> Result<(), BackendError> {
        self.request_action_with(action, Instant::now(), |backend, action, fresh_until| {
            let wda = UsbWda::new(backend.usb.clone()).with_write_deadline(fresh_until);
            backend
                .runtime
                .block_on(backend.actions.execute_existing(&wda, action))
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
        if now >= prepared.fresh_until {
            return Err(BackendError::Unavailable);
        }
        dispatch(self, action, prepared.fresh_until)
    }
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
        let fresh_until = started
            .checked_add(PIN_VIEW_BUDGET)
            .ok_or(BackendError::Unavailable)?;
        let (screen, view) = capture(self, started)?;
        if now() >= fresh_until {
            return Err(BackendError::Unavailable);
        }
        self.last_view = Some(PreparedPinView { view, fresh_until });
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
        Ok((screen, parsed))
    }
}

impl PasscodeBackend for NativePasscodeBackend {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
        self.snapshot_with(Instant::now(), Self::capture_snapshot, Instant::now)
    }

    fn show_keypad(&mut self) -> Result<(), BackendError> {
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
        self.last_view = None;
        let wda = UsbWda::new(self.usb.clone());
        self.runtime
            .block_on(self.actions.wake_hid_existing(&wda))
            .map_err(|_| BackendError::Unavailable)?;
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
        let at = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.keypad.as_ref())
            .and_then(|keys| keys.get(usize::from(digit)))
            .copied()
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&Action::Tap { at })
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
                let mut backend = backend();
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

    #[test]
    fn missing_or_expired_view_never_dispatches() {
        let action = Action::Tap {
            at: Point { x: 80.0, y: 340.0 },
        };
        let mut backend = backend();
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
