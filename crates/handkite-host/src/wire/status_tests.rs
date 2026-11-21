use super::*;
use handkite_core::controller::{AdapterError, RawObservation};
use handkite_core::protocol::{Action, DeviceBinding, HardwareId, ScreenImage};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Capture {
    observation: Observation,
    usb: HardwareId,
    wda: HardwareId,
    fail: bool,
    captures: usize,
    actions: usize,
}

struct Adapter(Arc<Mutex<Capture>>);

impl DeviceAdapter for Adapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        let capture = self.0.lock().unwrap();
        Ok((capture.usb.clone(), capture.wda.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        let mut capture = self.0.lock().unwrap();
        capture.captures += 1;
        if capture.fail {
            return Err(AdapterError);
        }
        let observation = &capture.observation;
        Ok(RawObservation {
            connected_usb: capture.usb.clone(),
            wda_target: capture.wda.clone(),
            window: observation.window.clone(),
            state: observation.state.clone(),
            accessibility_tree: observation.accessibility_tree.clone(),
            screenshot: observation.screenshot.clone(),
        })
    }

    fn execute(
        &mut self,
        action: &Action,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        assert!(Instant::now() < deadline);
        self.0.lock().unwrap().actions += 1;
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: action.kind().into(),
        })
    }
}

fn observation() -> Observation {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    Observation {
        device: DeviceKey::new("phone").unwrap(),
        revision: Revision(1),
        window: ObservationWindow {
            started_unix_ms: now,
            finished_unix_ms: now,
        },
        state: DeviceState {
            lock: LockState::Unlocked,
            wda_ready: true,
            foreground_app: None,
        },
        accessibility_tree: "<private-screen/>".into(),
        screenshot: ScreenImage {
            format: ImageFormat::Png,
            bytes: b"private-image".to_vec(),
        },
    }
}

fn fixture() -> (Controller<Adapter>, Arc<Mutex<Capture>>) {
    let usb = HardwareId::new("fixture-usb").unwrap();
    let capture = Arc::new(Mutex::new(Capture {
        observation: observation(),
        usb: usb.clone(),
        wda: usb.clone(),
        fail: false,
        captures: 0,
        actions: 0,
    }));
    let controller = Controller::new(
        DeviceBinding {
            key: DeviceKey::new("phone").unwrap(),
            usb_id: usb,
        },
        Adapter(capture.clone()),
        5_000,
        5_000,
    );
    (controller, capture)
}

fn status(controller: &Controller<Adapter>) -> Reply {
    dispatch(
        controller,
        Request::Status {
            device: DeviceKey::new("phone").unwrap(),
        },
    )
}

#[test]
fn status_is_one_full_capture_with_only_exact_metadata_on_the_wire() {
    let (controller, capture) = fixture();
    let expected = serde_json::to_value(WireStatus::from(
        capture.lock().unwrap().observation.clone(),
    ))
    .unwrap();
    let value = serde_json::to_value(status(&controller)).unwrap();
    assert_eq!(value, json!({"type":"status","status":expected}));
    assert_eq!(value["status"].as_object().unwrap().len(), 4);
    assert!(!value.to_string().contains("private"));
    let capture = capture.lock().unwrap();
    assert_eq!(capture.captures, 1);
    assert_eq!(capture.actions, 0);
}

#[test]
fn status_retains_the_complete_revision_for_action_comparison() {
    for change in ["none", "tree", "image"] {
        let (controller, capture) = fixture();
        let Reply::Status { status: first } = status(&controller) else {
            panic!("status failed")
        };
        {
            let mut capture = capture.lock().unwrap();
            match change {
                "tree" => capture.observation.accessibility_tree.push('x'),
                "image" => capture.observation.screenshot.bytes.push(0),
                _ => {}
            }
        }
        let lease = controller
            .acquire("fixture".into(), Duration::from_secs(30))
            .unwrap();
        let reply = dispatch(
            &controller,
            Request::Act {
                capture: CaptureMode::Paired,
                pacing: None,
                owner: "fixture".into(),
                lease,
                request: ActionRequest {
                    device: first.device,
                    expected_revision: Some(first.revision),
                    action: Action::Home,
                },
            },
        );
        let capture = capture.lock().unwrap();
        if change == "none" {
            let Reply::Acted {
                after: Some(after),
                receipt,
                ..
            } = reply
            else {
                panic!("action refused")
            };
            assert_eq!(receipt.accepted, Some(true));
            assert_eq!(after.revision, Revision(3));
            assert_eq!(capture.captures, 3);
            assert_eq!(capture.actions, 1);
        } else {
            assert!(matches!(reply, Reply::ViewChanged { .. }));
            assert_eq!(capture.captures, 2);
            assert_eq!(capture.actions, 0);
        }
    }
}

#[test]
fn status_is_fresh_and_has_the_same_capture_failures_as_observe() {
    for failure in [
        "adapter",
        "requested",
        "usb",
        "wda",
        "window",
        "duration",
        "stale",
        "future",
    ] {
        let (controller, capture) = fixture();
        {
            let mut capture = capture.lock().unwrap();
            match failure {
                "adapter" => capture.fail = true,
                "usb" => capture.usb = HardwareId::new("other").unwrap(),
                "wda" => capture.wda = HardwareId::new("other").unwrap(),
                "window" => capture.observation.window.started_unix_ms += 1,
                "duration" => capture.observation.window.started_unix_ms -= 5_001,
                "stale" => {
                    capture.observation.window.started_unix_ms -= 60_000;
                    capture.observation.window.finished_unix_ms -= 60_000;
                }
                "future" => {
                    capture.observation.window.started_unix_ms += 60_000;
                    capture.observation.window.finished_unix_ms += 60_000;
                }
                _ => {}
            }
        }
        let device = DeviceKey::new(if failure == "requested" {
            "other"
        } else {
            "phone"
        })
        .unwrap();
        let expected = match failure {
            "adapter" => "Adapter",
            "requested" | "usb" | "wda" => "DeviceMismatch",
            "window" | "duration" => "InvalidObservationWindow",
            _ => "StaleObservation",
        };
        for request in [
            Request::Status {
                device: device.clone(),
            },
            Request::Observe { device },
        ] {
            assert_eq!(
                serde_json::to_value(dispatch(&controller, request)).unwrap(),
                json!({"type":"error","code":expected}),
                "{failure}"
            );
        }
        assert_eq!(capture.lock().unwrap().captures, 2);
        assert_eq!(capture.lock().unwrap().actions, 0);
    }
    let (controller, capture) = fixture();
    let Reply::Status { status: first } = status(&controller) else {
        panic!("status failed")
    };
    capture.lock().unwrap().observation.state.foreground_app = Some("fixture.app".into());
    let Reply::Status { status: second } = status(&controller) else {
        panic!("status failed")
    };
    assert_eq!(first.revision, Revision(1));
    assert_eq!(second.revision, Revision(2));
    assert_eq!(second.state.foreground_app.as_deref(), Some("fixture.app"));
    assert_eq!(capture.lock().unwrap().captures, 2);
}

#[test]
fn status_requests_reject_unknown_duplicate_and_invalid_fields() {
    for input in [
        r#"{"method":"status","device":"../phone"}"#,
        r#"{"method":"status","device":"phone","tree_format":"compact"}"#,
        r#"{"method":"status","device":"phone","device":"other"}"#,
        r#"{"method":"status"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(input).is_err());
    }
    assert!(matches!(
        serde_json::from_str::<Request>(r#"{"method":"status","device":"phone"}"#).unwrap(),
        Request::Status { .. }
    ));
}

#[test]
fn synthetic_status_wire_size_is_independent_of_screen_payload() {
    let mut previous_status = None;
    for (tree_bytes, image_bytes) in [(16, 3), (32_929, 3 * 1024 * 1024)] {
        let mut observation = observation();
        observation.window = ObservationWindow {
            started_unix_ms: 1,
            finished_unix_ms: 2,
        };
        observation.accessibility_tree = "x".repeat(tree_bytes);
        observation.screenshot.bytes = vec![1; image_bytes];
        let full = Reply::Observed {
            observation: observation.clone().into(),
        };
        let status = Reply::Status {
            status: observation.into(),
        };
        let full_bytes = serde_json::to_vec(&full).unwrap();
        let status_bytes = serde_json::to_vec(&status).unwrap();
        if let Some(previous) = previous_status.as_ref() {
            assert_eq!(&status_bytes, previous);
        }
        previous_status = Some(status_bytes.clone());
        let full: Value = serde_json::from_slice(&full_bytes).unwrap();
        let status: Value = serde_json::from_slice(&status_bytes).unwrap();
        for field in ["device", "revision", "window", "state"] {
            assert_eq!(status["status"][field], full["observation"][field]);
        }
        assert!(status_bytes.len() < 256);
        assert!(full_bytes.len() > status_bytes.len());
        println!(
            "synthetic tree={tree_bytes} image={image_bytes} full_wire={} status_wire={}",
            full_bytes.len(),
            status_bytes.len()
        );
    }
}
