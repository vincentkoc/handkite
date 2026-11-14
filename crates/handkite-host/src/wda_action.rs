pub(crate) use crate::pin_field::valid_id as valid_session_id;
use crate::wda_http::HttpResponse;
use crate::wda_transport::{UsbWda, WdaError};
use handkite_core::controller::AdapterError;
use handkite_core::protocol::{Action, KeyboardKey, Point, TransportReceipt};
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) trait WdaRequest {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError>;
}

impl WdaRequest for UsbWda {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError> {
        UsbWda::request(self, method, path, body, deadline).await
    }
}

#[derive(Default)]
pub struct WdaActions {
    session: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionValidationError {
    pub endpoint: &'static str,
    pub reason: &'static str,
}

impl SessionValidationError {
    fn transport(endpoint: &'static str, error: WdaError) -> Self {
        Self {
            endpoint,
            reason: match error {
                WdaError::ExpiredBeforeSend => "expired_before_send",
                WdaError::TimedOutAfterSent => "timed_out_after_send",
                WdaError::AfterSent => "failed_after_send",
                WdaError::NotConnected => "not_connected",
                WdaError::Usb(_) => "usb",
                WdaError::InvalidRequest(_) => "invalid_request",
            },
        }
    }

    fn response(endpoint: &'static str, response: HttpResponse) -> Result<Value, Self> {
        if response.status != 200 {
            return Err(Self {
                endpoint,
                reason: "http_status",
            });
        }
        response_ok(response).map_err(|_| Self {
            endpoint,
            reason: "invalid_payload",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinateTapError {
    Http404,
    ReceiptUnproven,
}

impl WdaActions {
    pub(crate) fn bound_to_existing(session: &str) -> Result<Self, AdapterError> {
        if !valid_session_id(session) {
            return Err(AdapterError);
        }
        Ok(Self {
            session: Some(session.to_owned()),
        })
    }

    pub fn prepared_session_matches(&self, expected: &str) -> bool {
        self.session.as_deref() == Some(expected)
    }

    pub(crate) fn prepared_session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub async fn prepare_existing(&mut self, wda: &UsbWda) -> Result<(), AdapterError> {
        self.validate_existing(wda).await
    }

    async fn validate_existing(&mut self, wda: &impl WdaRequest) -> Result<(), AdapterError> {
        self.validate_existing_diagnostic(wda)
            .await
            .map_err(|_| AdapterError)
    }

    pub(crate) async fn validate_existing_diagnostic(
        &mut self,
        wda: &impl WdaRequest,
    ) -> Result<(), SessionValidationError> {
        let status = wda.request("GET", "/status", b"", Duration::from_secs(10));
        let (response, validation) = if let Some(id) = self.session.as_deref() {
            let path = format!("/session/{id}");
            let (status, session) = tokio::join!(
                status,
                wda.request("GET", &path, b"", Duration::from_secs(10))
            );
            (status, Some(session))
        } else {
            (status.await, None)
        };
        let response =
            response.map_err(|error| SessionValidationError::transport("status", error))?;
        let json = SessionValidationError::response("status", response)?;
        let id = json["sessionId"].as_str().ok_or(SessionValidationError {
            endpoint: "status",
            reason: "missing_session_id",
        })?;
        if !valid_session_id(id) {
            return Err(SessionValidationError {
                endpoint: "status",
                reason: "invalid_session_id",
            });
        }
        if self.session.as_ref().is_some_and(|current| current != id) {
            return Err(SessionValidationError {
                endpoint: "status",
                reason: "session_mismatch",
            });
        }
        let path = format!("/session/{id}");
        let validation = match validation {
            Some(response) => response,
            None => {
                wda.request("GET", &path, b"", Duration::from_secs(10))
                    .await
            }
        }
        .map_err(|error| SessionValidationError::transport("session", error))?;
        SessionValidationError::response("session", validation)?;
        self.session = Some(id.into());
        Ok(())
    }

    pub(crate) async fn execute_with_existing_session(
        &mut self,
        wda: &impl WdaRequest,
        action: &Action,
    ) -> Result<TransportReceipt, AdapterError> {
        self.validate_existing(wda).await?;
        let session = self.session.as_deref().ok_or(AdapterError)?;
        Self::execute_on(wda, action, session).await
    }

    /// Activates the Home screen once, without a button-press fallback.
    pub(crate) async fn activate_home_with_existing_session(
        &mut self,
        wda: &impl WdaRequest,
    ) -> Result<TransportReceipt, AdapterError> {
        if self.session.is_none() {
            return Err(AdapterError);
        }
        self.validate_existing(wda).await?;
        let response = wda
            .request("POST", "/wda/homescreen", b"{}", Duration::from_secs(30))
            .await
            .map_err(|_| AdapterError)?;
        if response_ok(response)?.get("value").is_none() {
            return Err(AdapterError);
        }
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: "home".into(),
        })
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
        Self::tap_on(wda, session, at).await
    }

    /// Fixed diagnostic categories only; HTTP 404 does not establish its cause.
    pub async fn tap_existing_diagnostic(
        &self,
        wda: &UsbWda,
        at: Point,
    ) -> Result<TransportReceipt, CoordinateTapError> {
        let session = self
            .session
            .as_deref()
            .ok_or(CoordinateTapError::ReceiptUnproven)?;
        Self::tap_on_diagnostic(wda, session, at).await
    }

    pub(crate) async fn tap_with_existing_session(
        &mut self,
        wda: &impl WdaRequest,
        at: Point,
    ) -> Result<TransportReceipt, AdapterError> {
        // This route requires an already bound session; never adopt one here.
        self.session.as_ref().ok_or(AdapterError)?;
        self.validate_existing(wda).await?;
        Self::tap_on(wda, self.session.as_deref().ok_or(AdapterError)?, at).await
    }

    async fn tap_on(
        wda: &impl WdaRequest,
        session: &str,
        at: Point,
    ) -> Result<TransportReceipt, AdapterError> {
        Self::tap_on_diagnostic(wda, session, at)
            .await
            .map_err(|_| AdapterError)
    }

    async fn tap_on_diagnostic(
        wda: &impl WdaRequest,
        session: &str,
        at: Point,
    ) -> Result<TransportReceipt, CoordinateTapError> {
        let body = coordinate_tap_payload(at).map_err(|_| CoordinateTapError::ReceiptUnproven)?;
        let path = format!("/session/{session}/wda/tap");
        let body = serde_json::to_vec(&body).map_err(|_| CoordinateTapError::ReceiptUnproven)?;
        let response = wda
            .request("POST", &path, &body, Duration::from_secs(30))
            .await
            .map_err(|_| CoordinateTapError::ReceiptUnproven)?;
        if response.status == 404 {
            return Err(CoordinateTapError::Http404);
        }
        response_ok(response).map_err(|_| CoordinateTapError::ReceiptUnproven)?;
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: "tap".into(),
        })
    }

    pub async fn wake_hid_existing(&self, wda: &UsbWda) -> Result<(), AdapterError> {
        let session = self.session.as_deref().ok_or(AdapterError)?;
        Self::wake_on(wda, session).await
    }

    pub(crate) async fn wake_with_existing_session(
        &mut self,
        wda: &impl WdaRequest,
    ) -> Result<(), AdapterError> {
        self.validate_existing(wda).await?;
        Self::wake_on(wda, self.session.as_deref().ok_or(AdapterError)?).await
    }

    async fn wake_on(wda: &impl WdaRequest, session: &str) -> Result<(), AdapterError> {
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
        wda: &impl WdaRequest,
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
            accepted: Some(true),
            action_kind: action.kind().into(),
        })
    }
}

fn response_ok(response: HttpResponse) -> Result<Value, AdapterError> {
    if response.status != 200 {
        return Err(AdapterError);
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    if !body
        .as_object()
        .is_some_and(|body| body.contains_key("value"))
        || body["value"].get("error").is_some()
    {
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
        Action::Text { value } | Action::TextCurrent { value } if value.len() <= 16_384 => {
            let chars: Vec<String> = value.chars().map(String::from).collect();
            Ok(("/wda/keys", json!({"value": chars})))
        }
        Action::Lock => Ok(("/wda/lock", json!({}))),
        Action::Tap { at } if point_valid(*at) => Ok((
            "/actions",
            touch_action(vec![
                json!({"type": "pointerMove", "duration": 0, "x": at.x, "y": at.y}),
                json!({"type": "pointerDown", "button": 0}),
                json!({"type": "pause", "duration": 80}),
                json!({"type": "pointerUp", "button": 0}),
            ]),
        )),
        Action::LongPress { at, duration_ms }
            if point_valid(*at) && (1..=5000).contains(duration_ms) =>
        {
            Ok((
                "/actions",
                touch_action(vec![
                    json!({"type":"pointerMove","duration":0,"x":at.x,"y":at.y}),
                    json!({"type":"pointerDown","button":0}),
                    json!({"type":"pause","duration":duration_ms}),
                    json!({"type":"pointerUp","button":0}),
                ]),
            ))
        }
        Action::DoubleTap { at } if point_valid(*at) => Ok((
            "/actions",
            touch_action(vec![
                json!({"type":"pointerMove","duration":0,"x":at.x,"y":at.y}),
                json!({"type":"pointerDown","button":0}),
                json!({"type":"pause","duration":80}),
                json!({"type":"pointerUp","button":0}),
                json!({"type":"pause","duration":100}),
                json!({"type":"pointerDown","button":0}),
                json!({"type":"pause","duration":80}),
                json!({"type":"pointerUp","button":0}),
            ]),
        )),
        Action::Key { key } | Action::KeyCurrent { key } => {
            let value = match key {
                KeyboardKey::Backspace => "\u{e003}",
                KeyboardKey::Enter => "\u{e007}",
                KeyboardKey::Tab => "\u{e004}",
                KeyboardKey::Escape => "\u{e00c}",
            };
            Ok((
                "/actions",
                json!({"actions":[{"type":"key","id":"keyboard",
                "actions":[{"type":"keyDown","value":value},{"type":"keyUp","value":value}]}]}),
            ))
        }
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
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[test]
    fn acknowledgments_require_a_wda_value_envelope() {
        for body in [
            json!({}),
            Value::Null,
            json!([]),
            json!(true),
            json!({"value":{"error":"unknown error"}}),
        ] {
            assert!(
                response_ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&body).unwrap()
                })
                .is_err()
            );
        }
        for body in [json!({"value":null}), json!({"value":{}})] {
            assert!(
                response_ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&body).unwrap()
                })
                .is_ok()
            );
        }
    }

    struct Step {
        method: &'static str,
        path: &'static str,
        response: Result<HttpResponse, WdaError>,
    }

    struct Script(RefCell<VecDeque<Step>>, Option<Value>);

    impl Script {
        fn new(steps: Vec<Step>) -> Self {
            Self(RefCell::new(steps.into()), None)
        }

        fn done(&self) {
            assert!(self.0.borrow().is_empty(), "unconsumed WDA requests");
        }
    }

    impl WdaRequest for Script {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _deadline: Duration,
        ) -> Result<HttpResponse, WdaError> {
            let step = self
                .0
                .borrow_mut()
                .pop_front()
                .expect("unexpected WDA request");
            assert_eq!((method, path), (step.method, step.path));
            if method == "GET" {
                assert!(body.is_empty());
            } else {
                assert_eq!(
                    serde_json::from_slice::<Value>(body).unwrap(),
                    self.1.clone().unwrap_or_else(|| json!({"name": "home"}))
                );
            }
            step.response
        }
    }

    fn response(method: &'static str, path: &'static str, status: u16, body: Value) -> Step {
        Step {
            method,
            path,
            response: Ok(HttpResponse {
                status,
                body: serde_json::to_vec(&body).unwrap(),
            }),
        }
    }

    fn status(id: Value) -> Step {
        response("GET", "/status", 200, json!({"sessionId": id, "value": {}}))
    }

    fn valid_action() -> Vec<Step> {
        vec![
            status(json!("fixture-session")),
            response("GET", "/session/fixture-session", 200, json!({"value": {}})),
            response(
                "POST",
                "/session/fixture-session/wda/pressButton",
                200,
                json!({"value": null}),
            ),
        ]
    }

    #[test]
    fn current_selection_text_uses_the_same_single_key_request() {
        let value = "Beta".to_owned();
        let strict = payload(&Action::Text {
            value: value.clone(),
        })
        .unwrap();
        let current = payload(&Action::TextCurrent { value }).unwrap();
        assert_eq!(current, strict);
        assert_eq!(current.0, "/wda/keys");
        assert_eq!(current.1, json!({"value":["B","e","t","a"]}));
    }

    #[tokio::test]
    async fn session_diagnostics_identify_failed_read_without_retry_or_rebinding() {
        let failures = [
            (WdaError::ExpiredBeforeSend, "expired_before_send"),
            (WdaError::AfterSent, "failed_after_send"),
            (WdaError::TimedOutAfterSent, "timed_out_after_send"),
            (WdaError::NotConnected, "not_connected"),
            (WdaError::Usb(crate::usb::UsbError::Unavailable), "usb"),
            (
                WdaError::InvalidRequest(crate::wda_http::HttpError::InvalidRequest),
                "invalid_request",
            ),
        ];
        for endpoint in ["status", "session"] {
            for (failure, reason) in failures {
                let mut steps = valid_action();
                steps.pop();
                steps[usize::from(endpoint == "session")].response = Err(failure);
                let script = Script::new(steps);
                let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
                assert_eq!(
                    actions.validate_existing_diagnostic(&script).await,
                    Err(SessionValidationError { endpoint, reason })
                );
                assert!(actions.prepared_session_matches("fixture-session"));
                script.done();
            }
            for (status, body, reason) in [
                (503, b"{}".as_slice(), "http_status"),
                (200, b"invalid-json".as_slice(), "invalid_payload"),
                (
                    200,
                    br#"{"value":{"error":"fixture"}}"#.as_slice(),
                    "invalid_payload",
                ),
            ] {
                let mut steps = valid_action();
                steps.pop();
                steps[usize::from(endpoint == "session")].response = Ok(HttpResponse {
                    status,
                    body: body.to_vec(),
                });
                let script = Script::new(steps);
                let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
                assert_eq!(
                    actions.validate_existing_diagnostic(&script).await,
                    Err(SessionValidationError { endpoint, reason })
                );
                assert!(actions.prepared_session_matches("fixture-session"));
                script.done();
            }
        }
        for (id, reason) in [
            (Value::Null, "missing_session_id"),
            (json!(""), "invalid_session_id"),
            (json!("different-session"), "session_mismatch"),
        ] {
            let script = Script::new(vec![
                status(id),
                response("GET", "/session/fixture-session", 200, json!({"value": {}})),
            ]);
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert_eq!(
                actions.validate_existing_diagnostic(&script).await,
                Err(SessionValidationError {
                    endpoint: "status",
                    reason
                })
            );
            assert!(actions.prepared_session_matches("fixture-session"));
            script.done();
        }
        let mut steps = valid_action();
        steps.pop();
        for step in &mut steps {
            step.response = Err(WdaError::ExpiredBeforeSend);
        }
        let script = Script::new(steps);
        let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
        assert_eq!(
            actions.validate_existing_diagnostic(&script).await,
            Err(SessionValidationError {
                endpoint: "status",
                reason: "expired_before_send"
            })
        );
        script.done();
    }

    #[tokio::test(start_paused = true)]
    async fn bound_validation_overlaps_reads_and_rejects_either_failure_before_input() {
        struct Reads {
            barrier: tokio::sync::Barrier,
            case: usize,
        }
        impl WdaRequest for Reads {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, WdaError> {
                assert_eq!(method, "GET", "invalid reads must prevent input");
                assert!(body.is_empty());
                self.barrier.wait().await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                let body = match path {
                    "/status" => match self.case {
                        1 => json!({"sessionId":"changed", "value":{}}),
                        2 => return Err(WdaError::NotConnected),
                        _ => json!({"sessionId":"fixture-session", "value":{}}),
                    },
                    "/session/fixture-session" => match self.case {
                        3 => json!({"value":{"error":"invalid session id"}}),
                        _ => json!({"value":{}}),
                    },
                    _ => panic!("unexpected read"),
                };
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&body).unwrap(),
                })
            }
        }
        for case in 0..4 {
            let reads = Reads {
                barrier: tokio::sync::Barrier::new(2),
                case,
            };
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            let started = tokio::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                if case == 0 {
                    actions.validate_existing(&reads).await
                } else {
                    actions
                        .execute_with_existing_session(&reads, &Action::Home)
                        .await
                        .map(|_| ())
                }
            })
            .await
            .expect("both reads must be polled together");
            assert_eq!(result.is_ok(), case == 0);
            assert_eq!(started.elapsed(), Duration::from_secs(1));
            assert_eq!(actions.session.as_deref(), Some("fixture-session"));
        }
    }

    #[tokio::test]
    async fn home_activation_validates_bound_session_and_sends_one_global_request() {
        for reply in [
            Ok(HttpResponse {
                status: 200,
                body: br#"{"value":null}"#.to_vec(),
            }),
            Ok(HttpResponse {
                status: 500,
                body: br#"{"value":null}"#.to_vec(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: br#"{"value":{"error":"fixture-error"}}"#.to_vec(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: b"not-json".to_vec(),
            }),
            Ok(HttpResponse {
                status: 200,
                body: b"{}".to_vec(),
            }),
            Err(WdaError::ExpiredBeforeSend),
            Err(WdaError::AfterSent),
            Err(WdaError::TimedOutAfterSent),
        ] {
            let success = reply
                .as_ref()
                .is_ok_and(|reply| reply.status == 200 && reply.body == br#"{"value":null}"#);
            let mut wda = Script::new(vec![
                status(json!("fixture-session")),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
                Step {
                    method: "POST",
                    path: "/wda/homescreen",
                    response: reply,
                },
            ]);
            wda.1 = Some(json!({}));
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            let receipt = actions.activate_home_with_existing_session(&wda).await;
            if success {
                let receipt = receipt.unwrap();
                assert_eq!(receipt.accepted, Some(true));
                assert_eq!(receipt.action_kind, "home");
            } else {
                assert!(receipt.is_err());
            }
            wda.done();
        }
    }

    #[tokio::test]
    async fn home_activation_refuses_unbound_or_changed_sessions_before_input() {
        let wda = Script::new(vec![]);
        assert!(
            WdaActions::default()
                .activate_home_with_existing_session(&wda)
                .await
                .is_err()
        );
        wda.done();
        let wda = Script::new(vec![
            status(json!("different-session")),
            response("GET", "/session/fixture-session", 200, json!({"value":{}})),
        ]);
        assert!(
            WdaActions::bound_to_existing("fixture-session")
                .unwrap()
                .activate_home_with_existing_session(&wda)
                .await
                .is_err()
        );
        wda.done();
        let wda = Script::new(vec![
            status(json!("fixture-session")),
            response("GET", "/session/fixture-session", 500, json!({"value":{}})),
        ]);
        assert!(
            WdaActions::bound_to_existing("fixture-session")
                .unwrap()
                .activate_home_with_existing_session(&wda)
                .await
                .is_err()
        );
        wda.done();
    }

    #[tokio::test]
    async fn coordinate_diagnostic_classifies_status_without_exposing_body_or_retrying() {
        let at = Point { x: 80.0, y: 340.0 };
        for (response, expected) in [
            (
                Ok(HttpResponse {
                    status: 404,
                    body: b"fixture-private-response".to_vec(),
                }),
                CoordinateTapError::Http404,
            ),
            (
                Ok(HttpResponse {
                    status: 500,
                    body: b"fixture-private-response".to_vec(),
                }),
                CoordinateTapError::ReceiptUnproven,
            ),
            (
                Ok(HttpResponse {
                    status: 200,
                    body: b"fixture-private-response".to_vec(),
                }),
                CoordinateTapError::ReceiptUnproven,
            ),
            (
                Ok(HttpResponse {
                    status: 200,
                    body: br#"{"value":{"error":"fixture-private-error"}}"#.to_vec(),
                }),
                CoordinateTapError::ReceiptUnproven,
            ),
            (
                Err(WdaError::ExpiredBeforeSend),
                CoordinateTapError::ReceiptUnproven,
            ),
            (
                Err(WdaError::AfterSent),
                CoordinateTapError::ReceiptUnproven,
            ),
            (
                Err(WdaError::TimedOutAfterSent),
                CoordinateTapError::ReceiptUnproven,
            ),
        ] {
            let mut wda = Script::new(vec![Step {
                method: "POST",
                path: "/session/fixture-session/wda/tap",
                response,
            }]);
            wda.1 = Some(json!({"x":80.0,"y":340.0}));
            assert_eq!(
                WdaActions::tap_on_diagnostic(&wda, "fixture-session", at)
                    .await
                    .err(),
                Some(expected)
            );
            wda.done();
        }
    }

    #[tokio::test]
    async fn bound_coordinate_tap_validates_session_and_never_falls_back() {
        let at = Point { x: 80.0, y: 340.0 };
        let wda = Script::new(vec![]);
        assert!(
            WdaActions::default()
                .tap_with_existing_session(&wda, at)
                .await
                .is_err()
        );
        wda.done();
        for steps in [
            vec![
                status(json!("different-valid-session")),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
            ],
            vec![
                status(json!("fixture-session")),
                response("GET", "/session/fixture-session", 404, json!({"value":{}})),
            ],
        ] {
            let wda = Script::new(steps);
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert!(actions.tap_with_existing_session(&wda, at).await.is_err());
            wda.done();
        }
        for failure in [
            None,
            Some(WdaError::ExpiredBeforeSend),
            Some(WdaError::AfterSent),
            Some(WdaError::TimedOutAfterSent),
        ] {
            let mut post = response(
                "POST",
                "/session/fixture-session/wda/tap",
                200,
                json!({"value":null}),
            );
            if let Some(error) = failure {
                post.response = Err(error);
            }
            let mut wda = Script::new(vec![
                status(json!("fixture-session")),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
                post,
            ]);
            wda.1 = Some(json!({"x":80.0,"y":340.0}));
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert_eq!(
                actions.tap_with_existing_session(&wda, at).await.is_err(),
                failure.is_some()
            );
            wda.done();
        }
    }

    #[tokio::test]
    async fn bound_wake_sends_one_existing_hid_event_even_with_uncertain_receipt() {
        for lost in [false, true] {
            let mut post = response(
                "POST",
                "/session/fixture-session/wda/performIoHidEvent",
                200,
                json!({"value": null}),
            );
            if lost {
                post.response = Err(WdaError::AfterSent);
            }
            let mut wda = Script::new(vec![
                status(json!("fixture-session")),
                response("GET", "/session/fixture-session", 200, json!({"value": {}})),
                post,
            ]);
            wda.1 = Some(json!({"page": 12, "usage": 547, "duration": 0.05}));
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert_eq!(
                actions.wake_with_existing_session(&wda).await.is_err(),
                lost
            );
            wda.done();
        }
    }

    #[tokio::test]
    async fn transaction_bound_inputs_refuse_a_different_valid_session_before_any_post() {
        for action in [
            Action::Tap {
                at: Point { x: 80.0, y: 340.0 },
            },
            Action::Tap {
                at: Point { x: 280.0, y: 640.0 },
            },
            Action::Swipe {
                from: Point { x: 200.0, y: 790.0 },
                to: Point { x: 200.0, y: 360.0 },
                duration_ms: 300,
            },
        ] {
            let wda = Script::new(vec![
                status(json!("different-valid-session")),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
            ]);
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert!(
                actions
                    .execute_with_existing_session(&wda, &action)
                    .await
                    .is_err()
            );
            wda.done();
        }
        let wda = Script::new(vec![
            status(json!("different-valid-session")),
            response("GET", "/session/fixture-session", 200, json!({"value":{}})),
        ]);
        let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
        assert!(actions.wake_with_existing_session(&wda).await.is_err());
        wda.done();
        assert!(WdaActions::bound_to_existing("../invalid").is_err());
    }

    #[tokio::test]
    async fn bound_input_revalidates_after_a_previous_success_without_retry() {
        let mut steps = valid_action();
        steps.push(status(json!("replacement-session")));
        steps.push(response(
            "GET",
            "/session/fixture-session",
            200,
            json!({"value":{}}),
        ));
        let wda = Script::new(steps);
        let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
        assert!(
            actions
                .execute_with_existing_session(&wda, &Action::Home)
                .await
                .is_ok()
        );
        assert!(
            actions
                .execute_with_existing_session(&wda, &Action::Home)
                .await
                .is_err()
        );
        wda.done();
    }

    #[tokio::test]
    async fn daemon_adopts_and_revalidates_the_existing_session_before_each_action() {
        let mut steps = valid_action();
        steps.extend(valid_action());
        let wda = Script::new(steps);
        let mut actions = WdaActions::default();
        for _ in 0..2 {
            let receipt = actions
                .execute_with_existing_session(&wda, &Action::Home)
                .await
                .unwrap();
            assert_eq!(receipt.accepted, Some(true));
            assert_eq!(receipt.action_kind, "home");
            assert_eq!(actions.session.as_deref(), Some("fixture-session"));
        }
        wda.done();
    }

    #[tokio::test]
    async fn daemon_rejects_absent_invalid_or_stale_sessions_before_input() {
        let cases = vec![
            vec![status(Value::Null)],
            vec![status(json!(""))],
            vec![status(json!("../invalid"))],
            vec![response("GET", "/status", 503, json!({"value": {}}))],
            vec![
                status(json!("fixture-session")),
                response(
                    "GET",
                    "/session/fixture-session",
                    404,
                    json!({"value": {"error": "invalid session id"}}),
                ),
            ],
            vec![
                status(json!("fixture-session")),
                response(
                    "GET",
                    "/session/fixture-session",
                    200,
                    json!({"value": {"error": "invalid session id"}}),
                ),
            ],
            vec![Step {
                method: "GET",
                path: "/status",
                response: Err(WdaError::NotConnected),
            }],
        ];
        for steps in cases {
            let wda = Script::new(steps);
            let mut actions = WdaActions::default();
            assert!(
                actions
                    .execute_with_existing_session(&wda, &Action::Home)
                    .await
                    .is_err()
            );
            assert!(actions.session.is_none());
            wda.done();
        }
    }

    #[tokio::test]
    async fn daemon_never_replaces_a_missing_changed_or_stale_pinned_session() {
        let cases = vec![
            vec![
                status(Value::Null),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
            ],
            vec![
                status(json!("replacement-session")),
                response("GET", "/session/fixture-session", 200, json!({"value":{}})),
            ],
            vec![
                status(json!("fixture-session")),
                response(
                    "GET",
                    "/session/fixture-session",
                    404,
                    json!({"value": {"error": "invalid session id"}}),
                ),
            ],
            vec![
                status(json!("fixture-session")),
                Step {
                    method: "GET",
                    path: "/session/fixture-session",
                    response: Err(WdaError::AfterSent),
                },
            ],
        ];
        for failure in cases {
            let mut steps = valid_action();
            steps.extend(failure);
            steps.push(status(json!("replacement-session")));
            steps.push(response(
                "GET",
                "/session/fixture-session",
                200,
                json!({"value":{}}),
            ));
            let wda = Script::new(steps);
            let mut actions = WdaActions::default();
            actions
                .execute_with_existing_session(&wda, &Action::Home)
                .await
                .unwrap();
            for _ in 0..2 {
                assert!(
                    actions
                        .execute_with_existing_session(&wda, &Action::Home)
                        .await
                        .is_err()
                );
                assert_eq!(actions.session.as_deref(), Some("fixture-session"));
            }
            wda.done();
        }
    }

    #[tokio::test]
    async fn daemon_does_not_retry_input_or_renew_after_lost_acknowledgment() {
        let mut steps = valid_action();
        steps.last_mut().unwrap().response = Err(WdaError::AfterSent);
        let wda = Script::new(steps);
        let mut actions = WdaActions::default();
        assert!(
            actions
                .execute_with_existing_session(&wda, &Action::Home)
                .await
                .is_err()
        );
        assert_eq!(actions.session.as_deref(), Some("fixture-session"));
        wda.done();
    }

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
    fn lock_uses_the_native_session_endpoint() {
        let (path, body) = payload(&Action::Lock).unwrap();
        assert_eq!(path, "/wda/lock");
        assert_eq!(body, json!({}));
    }

    #[tokio::test]
    async fn editing_inputs_are_balanced_single_requests_without_retry() {
        let at = Point { x: 25.5, y: 80.0 };
        let actions = [
            Action::DoubleTap { at },
            Action::LongPress {
                at,
                duration_ms: 750,
            },
            Action::Key {
                key: KeyboardKey::Backspace,
            },
            Action::Key {
                key: KeyboardKey::Enter,
            },
            Action::Key {
                key: KeyboardKey::Tab,
            },
            Action::Key {
                key: KeyboardKey::Escape,
            },
            Action::KeyCurrent {
                key: KeyboardKey::Backspace,
            },
            Action::KeyCurrent {
                key: KeyboardKey::Enter,
            },
        ];
        for action in actions {
            let (path, body) = payload(&action).unwrap();
            assert_eq!(path, "/actions");
            let steps = body["actions"][0]["actions"].as_array().unwrap();
            if let Action::Key { key } | Action::KeyCurrent { key } = action {
                let expected = match key {
                    KeyboardKey::Backspace => "\u{e003}",
                    KeyboardKey::Enter => "\u{e007}",
                    KeyboardKey::Tab => "\u{e004}",
                    KeyboardKey::Escape => "\u{e00c}",
                };
                assert_eq!(
                    steps.as_slice(),
                    [
                        json!({"type":"keyDown","value":expected}),
                        json!({"type":"keyUp","value":expected})
                    ]
                );
            } else {
                let downs = steps.iter().filter(|s| s["type"] == "pointerDown").count();
                let ups = steps.iter().filter(|s| s["type"] == "pointerUp").count();
                assert_eq!(
                    downs,
                    if matches!(action, Action::DoubleTap { .. }) {
                        2
                    } else {
                        1
                    }
                );
                assert_eq!(downs, ups);
                if matches!(action, Action::LongPress { .. }) {
                    assert_eq!(steps[2]["duration"], 750);
                }
            }
            for lost in [false, true] {
                let mut step = response(
                    "POST",
                    "/session/fixture-session/actions",
                    200,
                    json!({"value":null}),
                );
                if lost {
                    step.response = Err(WdaError::AfterSent);
                }
                let mut wda = Script::new(vec![step]);
                wda.1 = Some(body.clone());
                assert_eq!(
                    WdaActions::execute_on(&wda, &action, "fixture-session")
                        .await
                        .is_err(),
                    lost
                );
                wda.done();
            }
        }
        for duration_ms in [0, 5001] {
            assert!(payload(&Action::LongPress { at, duration_ms }).is_err());
        }
    }

    #[tokio::test]
    async fn native_lock_posts_once_to_bound_session_even_when_ack_is_lost() {
        for acknowledged in [false, true] {
            let mut steps = vec![
                status(json!("fixture-session")),
                response("GET", "/session/fixture-session", 200, json!({"value": {}})),
            ];
            steps.push(Step {
                method: "POST",
                path: "/session/fixture-session/wda/lock",
                response: if acknowledged {
                    Ok(HttpResponse {
                        status: 200,
                        body: serde_json::to_vec(&json!({"value": null})).unwrap(),
                    })
                } else {
                    Err(WdaError::NotConnected)
                },
            });
            let wda = Script(RefCell::new(steps.into()), Some(json!({})));
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            let result = actions
                .execute_with_existing_session(&wda, &Action::Lock)
                .await;
            assert_eq!(result.is_ok(), acknowledged);
            wda.done();
        }
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

    #[tokio::test]
    async fn touch_transports_send_exactly_one_post_even_when_acknowledgment_is_lost() {
        let point = Point { x: 250.0, y: 740.0 };
        let w3c = json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": [
            {"type": "pointerMove", "duration": 0, "x": 250.0, "y": 740.0},
            {"type": "pointerDown", "button": 0},
            {"type": "pause", "duration": 80},
            {"type": "pointerUp", "button": 0}
        ]}]});
        for coordinate in [false, true] {
            for lost in [false, true] {
                let path = if coordinate {
                    "/session/fixture-session/wda/tap"
                } else {
                    "/session/fixture-session/actions"
                };
                let mut step = response("POST", path, 200, json!({"value": null}));
                if lost {
                    step.response = Err(WdaError::AfterSent);
                }
                let mut wda = Script::new(vec![step]);
                wda.1 = Some(if coordinate {
                    json!({"x": 250.0, "y": 740.0})
                } else {
                    w3c.clone()
                });
                let result = if coordinate {
                    WdaActions::tap_on(&wda, "fixture-session", point).await
                } else {
                    WdaActions::execute_on(&wda, &Action::Tap { at: point }, "fixture-session")
                        .await
                };
                assert_eq!(result.is_err(), lost);
                wda.done();
            }
        }
    }

    #[tokio::test]
    async fn malformed_touch_acknowledgments_never_confirm_delivery_or_retry() {
        for body in [json!({}), Value::Null] {
            let wda = Script::new(vec![response(
                "POST",
                "/session/fixture-session/wda/tap",
                200,
                body,
            )]);
            let mut wda = wda;
            wda.1 = Some(json!({"x":250.0,"y":740.0}));
            assert!(
                WdaActions::tap_on(&wda, "fixture-session", Point { x: 250.0, y: 740.0 })
                    .await
                    .is_err()
            );
            wda.done();
        }
    }

    #[tokio::test]
    async fn swipe_preserves_direction_duration_and_sends_one_gesture() {
        let mut wda = Script::new(vec![response(
            "POST",
            "/session/fixture-session/actions",
            200,
            json!({"value":null}),
        )]);
        wda.1 = Some(touch_action(vec![
            json!({"type":"pointerMove","duration":0,"x":195.5,"y":700.0}),
            json!({"type":"pointerDown","button":0}),
            json!({"type":"pause","duration":50}),
            json!({"type":"pointerMove","duration":450,"x":195.5,"y":240.0}),
            json!({"type":"pointerUp","button":0}),
        ]));
        let receipt = WdaActions::execute_on(
            &wda,
            &Action::Swipe {
                from: Point { x: 195.5, y: 700.0 },
                to: Point { x: 195.5, y: 240.0 },
                duration_ms: 450,
            },
            "fixture-session",
        )
        .await
        .unwrap();
        assert_eq!(receipt.accepted, Some(true));
        assert_eq!(receipt.action_kind, "swipe");
        wda.done();
    }
}
