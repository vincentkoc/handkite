use crate::wda_http::HttpResponse;
use crate::wda_transport::{UsbWda, WdaError};
use handkite_core::controller::AdapterError;
use handkite_core::protocol::{Action, Point, TransportReceipt};
use serde_json::{Value, json};
use std::time::Duration;

const LOCK_SHORTCUT: &str = "shortcuts://run-shortcut?name=OpenClaw%20Lock%20Screen";

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

    pub async fn prepare_existing(&mut self, wda: &UsbWda) -> Result<(), AdapterError> {
        self.validate_existing(wda).await
    }

    async fn validate_existing(&mut self, wda: &impl WdaRequest) -> Result<(), AdapterError> {
        let response = wda
            .request("GET", "/status", b"", Duration::from_secs(10))
            .await
            .map_err(|_| AdapterError)?;
        let json = response_ok(response)?;
        let id = json["sessionId"].as_str().ok_or(AdapterError)?;
        if !valid_session_id(id) {
            return Err(AdapterError);
        }
        if self.session.as_ref().is_some_and(|current| current != id) {
            return Err(AdapterError);
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

    pub(crate) async fn execute_with_existing_session(
        &mut self,
        wda: &impl WdaRequest,
        action: &Action,
    ) -> Result<TransportReceipt, AdapterError> {
        self.validate_existing(wda).await?;
        let session = self.session.as_deref().ok_or(AdapterError)?;
        Self::execute_on(wda, action, session).await
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

    async fn tap_on(
        wda: &impl WdaRequest,
        session: &str,
        at: Point,
    ) -> Result<TransportReceipt, AdapterError> {
        let body = coordinate_tap_payload(at)?;
        let path = format!("/session/{session}/wda/tap/0");
        let body = serde_json::to_vec(&body).map_err(|_| AdapterError)?;
        let response = wda
            .request("POST", &path, &body, Duration::from_secs(30))
            .await
            .map_err(|_| AdapterError)?;
        response_ok(response)?;
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
    use std::cell::RefCell;
    use std::collections::VecDeque;

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
            let wda = Script::new(vec![status(json!("different-valid-session"))]);
            let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
            assert!(
                actions
                    .execute_with_existing_session(&wda, &action)
                    .await
                    .is_err()
            );
            wda.done();
        }
        let wda = Script::new(vec![status(json!("different-valid-session"))]);
        let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
        assert!(actions.wake_with_existing_session(&wda).await.is_err());
        wda.done();
        assert!(WdaActions::bound_to_existing("../invalid").is_err());
    }

    #[tokio::test]
    async fn bound_input_revalidates_after_a_previous_success_without_retry() {
        let mut steps = valid_action();
        steps.push(status(json!("replacement-session")));
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
            vec![status(Value::Null)],
            vec![status(json!("replacement-session"))],
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
                    "/session/fixture-session/wda/tap/0"
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
}
