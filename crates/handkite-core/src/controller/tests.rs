use super::*;
use crate::protocol::{ImageFormat, LockState, Point, ScreenImage};

struct FakeAdapter {
    usb: HardwareId,
    wda: HardwareId,
    app: String,
    stale_capture: bool,
    fail_capture: bool,
    fail_after_action: bool,
    locked: bool,
    actions: usize,
}

impl FakeAdapter {
    fn new() -> Self {
        let id = HardwareId::new("fixture-phone-a").unwrap();
        Self {
            usb: id.clone(),
            wda: id,
            app: "com.example.notes".into(),
            stale_capture: false,
            fail_capture: false,
            fail_after_action: false,
            locked: false,
            actions: 0,
        }
    }
}

impl DeviceAdapter for FakeAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        Ok((self.usb.clone(), self.wda.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        if self.fail_capture {
            return Err(AdapterError);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let finished = if self.stale_capture {
            now - 20_000
        } else {
            now
        };
        Ok(RawObservation {
            connected_usb: self.usb.clone(),
            wda_target: self.wda.clone(),
            window: ObservationWindow {
                started_unix_ms: finished - 20,
                finished_unix_ms: finished,
            },
            state: DeviceState {
                lock: if self.locked {
                    LockState::Locked
                } else {
                    LockState::Unlocked
                },
                wda_ready: true,
                foreground_app: Some(self.app.clone()),
            },
            accessibility_tree: format!("<app name=\"{}\" />", self.app),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: self.app.as_bytes().to_vec(),
            },
        })
    }

    fn execute(&mut self, action: &Action) -> Result<TransportReceipt, AdapterError> {
        self.actions += 1;
        if self.fail_after_action {
            self.fail_capture = true;
        }
        if matches!(action, Action::Home) {
            self.app = "com.apple.springboard".into();
        }
        Ok(TransportReceipt {
            accepted: true,
            action_kind: action.kind().into(),
        })
    }
}

fn controller() -> (Controller<FakeAdapter>, DeviceKey) {
    let key = DeviceKey::new("fixture-phone").unwrap();
    let binding = DeviceBinding {
        key: key.clone(),
        usb_id: HardwareId::new("fixture-phone-a").unwrap(),
    };
    (
        Controller::new(binding, FakeAdapter::new(), 1000, 5000),
        key,
    )
}

#[test]
fn lease_rejects_a_competing_owner_and_a_stale_revision() {
    let (controller, key) = controller();
    let owner = "agent-a";
    let lease = controller
        .acquire(owner.into(), Duration::from_secs(30))
        .unwrap();
    assert_eq!(
        controller.acquire("agent-b".into(), Duration::from_secs(30)),
        Err(ControllerError::Busy)
    );

    let before = controller.observe(&key).unwrap();
    let request = ActionRequest {
        device: key.clone(),
        expected_revision: Some(before.revision),
        action: Action::Home,
    };
    let result = controller.act(owner, lease, request).unwrap();
    assert_eq!(result.effect, ObservedEffect::Changed);
    assert_eq!(result.after.as_ref().unwrap().revision, Revision(3));
    assert_eq!(result.receipt.action_kind, "home");

    let stale = ActionRequest {
        device: key.clone(),
        expected_revision: Some(before.revision),
        action: Action::Home,
    };
    assert_eq!(
        controller.act(owner, lease, stale).err(),
        Some(ControllerError::StaleRevision)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 1);
    assert_eq!(
        controller.release("agent-b", lease),
        Err(ControllerError::InvalidLease)
    );
    controller.release(owner, lease).unwrap();
}

#[test]
fn target_change_blocks_an_action_before_transport() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    controller.inner.lock().unwrap().adapter.wda = HardwareId::new("fixture-phone-b").unwrap();
    let request = ActionRequest {
        device: key,
        expected_revision: None,
        action: Action::Tap {
            at: Point { x: 20.0, y: 30.0 },
        },
    };
    assert_eq!(
        controller.act("agent", lease, request).err(),
        Some(ControllerError::DeviceMismatch)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
}

#[test]
fn missing_after_capture_is_not_reported_as_a_confirmed_effect() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    let before = controller.observe(&key).unwrap();
    controller.inner.lock().unwrap().adapter.fail_after_action = true;
    let request = ActionRequest {
        device: key.clone(),
        expected_revision: Some(before.revision),
        action: Action::Home,
    };
    let result = controller.act("agent", lease, request).unwrap();
    assert_eq!(result.effect, ObservedEffect::Unconfirmed);
    assert!(result.after.is_none());

    let stale = ActionRequest {
        device: key,
        expected_revision: Some(before.revision),
        action: Action::Home,
    };
    assert_eq!(
        controller.act("agent", lease, stale).err(),
        Some(ControllerError::StaleRevision)
    );
}

#[test]
fn old_screen_capture_is_rejected() {
    let (controller, key) = controller();
    controller.inner.lock().unwrap().adapter.stale_capture = true;
    assert_eq!(
        controller.observe(&key).err(),
        Some(ControllerError::StaleObservation)
    );
}

#[test]
fn changed_screen_or_lock_state_blocks_input_before_transport() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    let before = controller.observe(&key).unwrap();
    controller.inner.lock().unwrap().adapter.app = "com.example.other".into();
    let request = ActionRequest {
        device: key.clone(),
        expected_revision: Some(before.revision),
        action: Action::Tap {
            at: Point { x: 20.0, y: 30.0 },
        },
    };
    assert_eq!(
        controller.act("agent", lease, request).err(),
        Some(ControllerError::StaleRevision)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);

    controller.inner.lock().unwrap().adapter.locked = true;
    let request = ActionRequest {
        device: key,
        expected_revision: None,
        action: Action::Text {
            value: "fixture".into(),
        },
    };
    assert_eq!(
        controller.act("agent", lease, request).err(),
        Some(ControllerError::LockedAction)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
}
