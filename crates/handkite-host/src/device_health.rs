use crate::native_lock;
use crate::usb::PinnedUsb;
use crate::wda_action::WdaActions;
use crate::wda_http::HttpResponse;
use crate::wda_transport::UsbWda;
use handkite_core::protocol::{HardwareId, LockState};
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DeviceHealth {
    pub usb_connected: bool,
    pub wda_http: bool,
    pub wda_ready: bool,
    pub existing_session_valid: bool,
    pub native_lock: LockState,
    pub unlocked_since_boot: Option<bool>,
}

impl DeviceHealth {
    pub fn observation_ready(&self) -> bool {
        self.usb_connected
            && self.wda_http
            && self.wda_ready
            && self.existing_session_valid
            && self.native_lock != LockState::Uncertain
    }
}

pub fn inspect(expected: HardwareId) -> DeviceHealth {
    let mut result = DeviceHealth {
        usb_connected: false,
        wda_http: false,
        wda_ready: false,
        existing_session_valid: false,
        native_lock: LockState::Uncertain,
        unlocked_since_boot: None,
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return result;
    };
    let usb = PinnedUsb::new(expected.clone());
    result.usb_connected = runtime.block_on(usb.provider()).is_ok();
    if !result.usb_connected {
        return result;
    }
    let wda = UsbWda::new(usb);
    if let Ok(response) =
        runtime.block_on(wda.request("GET", "/status", b"", Duration::from_secs(8)))
    {
        result.wda_http = response.status == 200;
        result.wda_ready = parse_ready(&response);
    }
    if result.wda_ready {
        let mut actions = WdaActions::default();
        result.existing_session_valid = runtime.block_on(actions.prepare_existing(&wda)).is_ok();
    }
    let native = native_lock::query(&expected);
    result.native_lock = native.state;
    result.unlocked_since_boot = native.unlocked_since_boot;
    result
}

fn parse_ready(response: &HttpResponse) -> bool {
    response.status == 200
        && serde_json::from_slice::<serde_json::Value>(&response.body)
            .ok()
            .is_some_and(|json| json["value"]["ready"] == true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_a_typed_successful_response() {
        let ready = HttpResponse {
            status: 200,
            body: br#"{"value":{"ready":true}}"#.to_vec(),
        };
        assert!(parse_ready(&ready));
        assert!(!parse_ready(&HttpResponse {
            status: 503,
            body: br#"{"value":{"ready":true}}"#.to_vec(),
        }));
        assert!(!parse_ready(&HttpResponse {
            status: 200,
            body: br#"{"value":{"ready":"true"}}"#.to_vec(),
        }));
    }
}
