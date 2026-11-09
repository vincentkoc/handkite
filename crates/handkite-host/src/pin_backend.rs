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

pub struct NativePasscodeBackend {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
    last_view: Option<ParsedPasscodeView>,
}

impl NativePasscodeBackend {
    pub fn new(expected: HardwareId) -> Result<Self, BackendError> {
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
        })
    }

    fn request_action(&mut self, action: &Action) -> Result<(), BackendError> {
        let wda = UsbWda::new(self.usb.clone());
        self.runtime
            .block_on(self.actions.execute(&wda, action))
            .map_err(|_| BackendError::Unavailable)?;
        self.last_view = None;
        Ok(())
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

impl PasscodeBackend for NativePasscodeBackend {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
        let started = Instant::now();
        let wda = UsbWda::new(self.usb.clone());
        self.runtime
            .block_on(self.actions.prepare(&wda))
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
        if started.elapsed() > Duration::from_secs(5) {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        let wda_lock = match reported_lock.as_bool() {
            Some(true) => LockState::Locked,
            Some(false) => LockState::Unlocked,
            None => LockState::Uncertain,
        };
        let screen = PasscodeScreen {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            native_lock: native.state,
            visible_lock: parsed.visible_lock,
            wda_lock,
            keypad_ready: parsed.keypad.is_some() && parsed.secure_field_value.is_some(),
            secure_field_value: parsed.secure_field_value.clone(),
            before_first_unlock: native.unlocked_since_boot != Some(true),
            trust_prompt: parsed.has_alert,
        };
        self.last_view = Some(parsed);
        Ok(screen)
    }

    fn show_keypad(&mut self) -> Result<(), BackendError> {
        if self
            .last_view
            .as_ref()
            .is_some_and(|view| view.keypad.is_some())
        {
            return Ok(());
        }
        self.request_action(&Action::Home)?;
        thread::sleep(Duration::from_millis(300));
        let screen = self.snapshot()?;
        if screen.keypad_ready {
            return Ok(());
        }
        let size = self
            .last_view
            .as_ref()
            .and_then(|view| view.screen_size)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&keypad_swipe(size)?)?;
        for _ in 0..8 {
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
            .and_then(|view| view.keypad.as_ref())
            .and_then(|keys| keys.get(usize::from(digit)))
            .copied()
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&Action::Tap { at })
    }

    fn delete_digit(&mut self) -> Result<(), BackendError> {
        let at = self
            .last_view
            .as_ref()
            .and_then(|view| view.delete_key)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&Action::Tap { at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
