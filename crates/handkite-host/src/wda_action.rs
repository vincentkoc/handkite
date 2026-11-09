use crate::wda_http::HttpResponse;
use crate::wda_transport::UsbWda;
use handkite_core::controller::AdapterError;
use handkite_core::protocol::{Action, Point, TransportReceipt};
use serde_json::{Value, json};
use std::time::Duration;

const LOCK_SHORTCUT: &str = "shortcuts://run-shortcut?name=OpenClaw%20Lock%20Screen";

#[derive(Default)]
pub struct WdaActions {
    session: Option<String>,
}

impl WdaActions {
    pub async fn prepare_existing(&mut self, wda: &UsbWda) -> Result<(), AdapterError> {
        let response = wda
            .request("GET", "/status", b"", Duration::from_secs(10))
            .await
            .map_err(|_| AdapterError)?;
        let json = response_ok(response)?;
        let id = json["sessionId"].as_str().ok_or(AdapterError)?;
        if !valid_session_id(id) {
            return Err(AdapterError);
        }
        if let Some(current) = &self.session {
            return (current == id).then_some(()).ok_or(AdapterError);
        }
        let path = format!("/session/{id}");
        let validation = wda
            .request("GET", &path, b"", Duration::from_secs(10))
            .await
            .map_err(|_| AdapterError)?;
        response_ok(validation)?;
        self.session = Some(id.into());
        Ok(())
    }

    pub async fn execute(
        &mut self,
        wda: &UsbWda,
        action: &Action,
    ) -> Result<TransportReceipt, AdapterError> {
        let session = self.session_id(wda).await?;
        Self::execute_on(wda, action, &session).await
    }

    pub async fn execute_existing(
        &self,
        wda: &UsbWda,
        action: &Action,
    ) -> Result<TransportReceipt, AdapterError> {
        let session = self.session.as_deref().ok_or(AdapterError)?;
        Self::execute_on(wda, action, session).await
    }

    /// A coordinate tap through WDA's dedicated endpoint. Qualify on harmless UI
    /// before selecting it for passcode entry.
    pub async fn tap_existing(
        &self,
        wda: &UsbWda,
        at: Point,
    ) -> Result<TransportReceipt, AdapterError> {
        let session = self.session.as_deref().ok_or(AdapterError)?;
        let body = coordinate_tap_payload(at)?;
        let path = format!("/session/{session}/wda/tap/0");
        let body = serde_json::to_vec(&body).map_err(|_| AdapterError)?;
        let response = wda
            .request("POST", &path, &body, Duration::from_secs(30))
            .await
            .map_err(|_| AdapterError)?;
        response_ok(response)?;
        Ok(TransportReceipt {
            accepted: true,
            action_kind: "tap".into(),
        })
    }

    pub async fn wake_hid_existing(&self, wda: &UsbWda) -> Result<(), AdapterError> {
        let session = self.session.as_deref().ok_or(AdapterError)?;
        let path = format!("/session/{session}/wda/performIoHidEvent");
        let body = serde_json::to_vec(&json!({"page": 0x0c, "usage": 0x223, "duration": 0.05}))
            .map_err(|_| AdapterError)?;
        let response = wda
            .request("POST", &path, &body, Duration::from_secs(10))
            .await
            .map_err(|_| AdapterError)?;
        response_ok(response)?;
        Ok(())
    }

    async fn execute_on(
        wda: &UsbWda,
        action: &Action,
        session: &str,
    ) -> Result<TransportReceipt, AdapterError> {
        let (endpoint, body) = payload(action)?;
        let path = format!("/session/{session}{endpoint}");
        let body = serde_json::to_vec(&body).map_err(|_| AdapterError)?;
        let response = wda
            .request("POST", &path, &body, Duration::from_secs(30))
            .await
            .map_err(|_| AdapterError)?;
        response_ok(response)?;
        Ok(TransportReceipt {
            accepted: true,
            action_kind: action.kind().into(),
        })
    }

    async fn session_id(&mut self, wda: &UsbWda) -> Result<String, AdapterError> {
        if let Some(session) = &self.session {
            return Ok(session.clone());
        }
        let body = serde_json::to_vec(&json!({"capabilities": {"alwaysMatch": {}}}))
            .map_err(|_| AdapterError)?;
        let response = wda
            .request("POST", "/session", &body, Duration::from_secs(30))
            .await
            .map_err(|_| AdapterError)?;
        let json = response_ok(response)?;
        let id = json["sessionId"]
            .as_str()
            .or_else(|| json["value"]["sessionId"].as_str())
            .ok_or(AdapterError)?;
        if !valid_session_id(id) {
            return Err(AdapterError);
        }
        self.session = Some(id.into());
        Ok(id.into())
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn response_ok(response: HttpResponse) -> Result<Value, AdapterError> {
    if response.status != 200 {
        return Err(AdapterError);
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    if body["value"].get("error").is_some() {
        return Err(AdapterError);
    }
    Ok(body)
}

fn point_valid(point: Point) -> bool {
    point.x.is_finite()
        && point.y.is_finite()
        && (0.0..=10_000.0).contains(&point.x)
        && (0.0..=10_000.0).contains(&point.y)
}

fn coordinate_tap_payload(at: Point) -> Result<Value, AdapterError> {
    point_valid(at)
        .then(|| json!({"x": at.x, "y": at.y}))
        .ok_or(AdapterError)
}

fn touch_action(steps: Vec<Value>) -> Value {
    json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": steps}]})
}

fn payload(action: &Action) -> Result<(&'static str, Value), AdapterError> {
    match action {
        Action::Home => Ok(("/wda/pressButton", json!({"name": "home"}))),
        Action::LaunchApp { bundle_id } if !bundle_id.is_empty() && bundle_id.len() <= 256 => {
            Ok(("/wda/apps/launch", json!({"bundleId": bundle_id})))
        }
        Action::Text { value } if value.len() <= 16_384 => {
            let chars: Vec<String> = value.chars().map(String::from).collect();
            Ok(("/wda/keys", json!({"value": chars})))
        }
        Action::Lock => Ok(("/url", json!({"url": LOCK_SHORTCUT}))),
        Action::Tap { at } if point_valid(*at) => Ok((
            "/actions",
            touch_action(vec![
                json!({"type": "pointerMove", "duration": 0, "x": at.x, "y": at.y}),
                json!({"type": "pointerDown", "button": 0}),
                json!({"type": "pause", "duration": 80}),
                json!({"type": "pointerUp", "button": 0}),
            ]),
        )),
        Action::Swipe {
            from,
            to,
            duration_ms,
        } if point_valid(*from) && point_valid(*to) && (1..=5000).contains(duration_ms) => Ok((
            "/actions",
            touch_action(vec![
                json!({"type": "pointerMove", "duration": 0, "x": from.x, "y": from.y}),
                json!({"type": "pointerDown", "button": 0}),
                json!({"type": "pause", "duration": 50}),
                json!({"type": "pointerMove", "duration": duration_ms, "x": to.x, "y": to.y}),
                json!({"type": "pointerUp", "button": 0}),
            ]),
        )),
        _ => Err(AdapterError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::PinnedUsb;
    use handkite_core::protocol::HardwareId;

    #[tokio::test]
    async fn pin_action_refuses_to_create_a_session() {
        let wda = UsbWda::new(PinnedUsb::new(HardwareId::new("fixture-device").unwrap()));
        assert!(
            WdaActions::default()
                .execute_existing(&wda, &Action::Home)
                .await
                .is_err()
        );
        assert!(
            WdaActions::default()
                .tap_existing(&wda, Point { x: 100.0, y: 200.0 })
                .await
                .is_err()
        );
    }

    #[test]
    fn lock_uses_the_verified_shortcut() {
        let (path, body) = payload(&Action::Lock).unwrap();
        assert_eq!(path, "/url");
        assert_eq!(body["url"], LOCK_SHORTCUT);
    }

    #[test]
    fn invalid_coordinates_never_reach_wda() {
        assert_eq!(
            coordinate_tap_payload(Point { x: 120.0, y: 240.0 }).unwrap(),
            json!({"x": 120.0, "y": 240.0})
        );
        assert!(
            coordinate_tap_payload(Point {
                x: f64::NAN,
                y: 0.0
            })
            .is_err()
        );
        assert!(
            payload(&Action::Tap {
                at: Point {
                    x: f64::NAN,
                    y: 0.0
                }
            })
            .is_err()
        );
        assert!(
            payload(&Action::Swipe {
                from: Point { x: 0.0, y: 0.0 },
                to: Point { x: 1.0, y: 1.0 },
                duration_ms: 0
            })
            .is_err()
        );
    }
}
