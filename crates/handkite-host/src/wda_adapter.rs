use crate::native_lock;
use crate::usb::PinnedUsb;
use crate::wda_action::WdaActions;
use crate::wda_http::HttpResponse;
use crate::wda_runner::OwnedWdaRunner;
use crate::wda_session::{SessionClose, close_owned_session, create_owned_session};
use crate::wda_settings::validate_session;
use crate::wda_transport::UsbWda;
use base64::Engine;
use handkite_core::controller::{AdapterError, DeviceAdapter, RawObservation};
use handkite_core::protocol::{
    Action, DeviceState, HardwareId, ImageFormat, LockState, ObservationWindow, ScreenImage,
    TransportReceipt,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SCREEN_SOURCE: &str = "/source?format=xml&excluded_attributes=visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const ATTRIBUTE_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";

pub struct WdaAdapter {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
    owned_runner: Option<OwnedWdaRunner>,
    owned_session: Option<String>,
}

impl WdaAdapter {
    pub fn new(expected: HardwareId) -> Result<Self, AdapterError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AdapterError)?;
        Ok(Self {
            usb: PinnedUsb::new(expected.clone()),
            expected,
            runtime,
            actions: WdaActions::default(),
            owned_runner: None,
            owned_session: None,
        })
    }

    /// Start one configured XCTest runner and create one exact WDA session.
    /// The caller must first obtain exclusive device and WDA ownership.
    /// The multi-thread runtime continues driving XCTest while the socket server is idle.
    pub fn new_owned(expected: HardwareId, runner_bundle_id: &str) -> Result<Self, AdapterError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|_| AdapterError)?;
        let usb = PinnedUsb::new(expected.clone());
        let runner = runtime
            .block_on(OwnedWdaRunner::start(
                usb.clone(),
                runner_bundle_id,
                Duration::from_secs(120),
            ))
            .map_err(|_| AdapterError)?;
        let session = runtime
            .block_on(create_owned_session(&UsbWda::new(usb.clone()), || Ok(())))
            .map_err(|_| AdapterError)?;
        let actions = WdaActions::bound_to_existing(&session)?;
        Ok(Self {
            expected,
            usb,
            runtime,
            actions,
            owned_runner: Some(runner),
            owned_session: Some(session),
        })
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
        self.validate_owned()?;
        let captured =
            self.runtime
                .block_on(Self::capture(&self.usb, &self.expected, source_path))?;
        self.validate_owned()?;
        Ok(captured)
    }

    async fn capture(
        usb: &PinnedUsb,
        expected: &HardwareId,
        source_path: &str,
    ) -> Result<(RawObservation, Instant), AdapterError> {
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
        let lock = classify_lock(&app, &tree, reported_lock.as_bool());
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
        Ok((observation, finished))
    }

    /// Diagnostic capture retaining visibility, enabled and selected attributes.
    /// The monotonic completion anchor bounds later diagnostic input freshness.
    /// Ordinary controller observations continue to use the smaller projection.
    pub fn observe_with_attributes(&mut self) -> Result<(RawObservation, Instant), AdapterError> {
        self.capture_checked(ATTRIBUTE_SOURCE)
    }

    /// Paired diagnostic capture with the PIN parser's visible/enabled attributes.
    pub fn observe_pin_with_attributes(
        &mut self,
    ) -> Result<(RawObservation, Instant), AdapterError> {
        self.capture_checked(PIN_SOURCE)
    }
}

impl DeviceAdapter for WdaAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        self.validate_owned()?;
        self.runtime
            .block_on(self.usb.provider())
            .map_err(|_| AdapterError)?;
        Ok((self.expected.clone(), self.expected.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.capture_checked(SCREEN_SOURCE)
            .map(|(observation, _)| observation)
    }

    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.validate_owned()?;
        let wda = UsbWda::new(self.usb.clone()).with_write_deadline(send_deadline);
        let receipt = self
            .runtime
            .block_on(self.actions.execute_with_existing_session(&wda, action))?;
        self.validate_owned()?;
        Ok(receipt)
    }

    fn native_lock_state(&mut self) -> LockState {
        native_lock::query(&self.expected).state
    }
}

impl Drop for WdaAdapter {
    fn drop(&mut self) {
        if let Some(runner) = self.owned_runner.take() {
            if let Some(session) = self.owned_session.take() {
                if runner.is_finished() {
                    eprintln!("handkite: owned runner exited before session close");
                } else if self.runtime.block_on(close_owned_session(
                    &UsbWda::new(self.usb.clone()),
                    &session,
                )) != SessionClose::Acknowledged
                {
                    eprintln!("handkite: owned session close unproven");
                }
            }
            if self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), runner.abort_and_join()).await
            }) != Ok(Ok(()))
            {
                eprintln!("handkite: owned runner host task termination unproven");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
