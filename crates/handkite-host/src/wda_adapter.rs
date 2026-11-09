use crate::usb::PinnedUsb;
use crate::wda_action::WdaActions;
use crate::wda_http::HttpResponse;
use crate::wda_transport::UsbWda;
use base64::Engine;
use handkite_core::controller::{AdapterError, DeviceAdapter, RawObservation};
use handkite_core::protocol::{
    Action, DeviceState, HardwareId, ImageFormat, LockState, ObservationWindow, ScreenImage,
    TransportReceipt,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCREEN_SOURCE: &str = "/source?format=xml&excluded_attributes=visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";

pub struct WdaAdapter {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
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
        })
    }

    async fn capture(
        usb: &PinnedUsb,
        expected: &HardwareId,
    ) -> Result<RawObservation, AdapterError> {
        let started_unix_ms = unix_ms()?;
        let wda = UsbWda::new(usb.clone());
        let (image, source, app, reported_lock) = tokio::join!(
            wda.request("GET", "/screenshot", b"", Duration::from_secs(20)),
            wda.request("GET", SCREEN_SOURCE, b"", Duration::from_secs(20)),
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
        Ok(RawObservation {
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
        })
    }
}

impl DeviceAdapter for WdaAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        self.runtime
            .block_on(self.usb.provider())
            .map_err(|_| AdapterError)?;
        Ok((self.expected.clone(), self.expected.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.runtime
            .block_on(Self::capture(&self.usb, &self.expected))
    }

    fn execute(&mut self, action: &Action) -> Result<TransportReceipt, AdapterError> {
        self.runtime
            .block_on(self.actions.execute(&UsbWda::new(self.usb.clone()), action))
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
