use super::*;
use crate::protocol::{ImageFormat, LockState, Point, ScreenImage};

struct FakeAdapter {
    usb: HardwareId,
    wda: HardwareId,
    app: String,
    stale_capture: bool,
    fail_capture: bool,
    fail_after_action: bool,
    lose_ack: bool,
    mismatch_after_action: bool,
    locked: bool,
    actions: usize,
    native_lock: LockState,
    native_queries: usize,
    capture_wait_until: Option<Instant>,
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
            lose_ack: false,
            mismatch_after_action: false,
            locked: false,
            actions: 0,
            native_lock: LockState::Uncertain,
            native_queries: 0,
            capture_wait_until: None,
        }
    }
}

impl DeviceAdapter for FakeAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        Ok((self.usb.clone(), self.wda.clone()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        if let Some(deadline) = self.capture_wait_until.take() {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
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
        if matches!(action, Action::Lock) {
            self.app = "com.apple.shortcuts".into();
        }
        if self.mismatch_after_action {
            self.wda = HardwareId::new("fixture-phone-b").unwrap();
        }
        if self.lose_ack {
            return Err(AdapterError);
        }
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: action.kind().into(),
        })
    }

    fn native_lock_state(&mut self) -> LockState {
        assert_eq!(self.actions, 1);
        self.native_queries += 1;
        self.native_lock
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
fn lease_token_is_opaque_and_a_guess_cannot_take_control() {
    let (controller, key) = controller();
    let first = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    let token = serde_json::to_value(first).unwrap();
    assert_eq!(token.as_str().unwrap().len(), 32);
    assert!(!format!("{first:?}").contains(token.as_str().unwrap()));

    let guess = LeaseId::try_from("00000000000000000000000000000001".to_owned()).unwrap();
    let request = ActionRequest {
        device: key,
        expected_revision: None,
        action: Action::Home,
    };
    assert_eq!(
        controller.act("agent", guess, request).err(),
        Some(ControllerError::InvalidLease)
    );
    assert_eq!(
        controller.release("agent", guess),
        Err(ControllerError::InvalidLease)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);

    controller.release("agent", first).unwrap();
    let second = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    assert_ne!(first, second);
}

#[test]
fn lease_expiring_during_capture_cannot_dispatch_input() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_millis(250))
        .unwrap();
    {
        let mut inner = controller.inner.lock().unwrap();
        inner.adapter.capture_wait_until = Some(inner.lease.as_ref().unwrap().expires_at);
    }
    let request = || ActionRequest {
        device: key.clone(),
        expected_revision: None,
        action: Action::Home,
    };
    assert_eq!(
        controller.act("agent", lease, request()).err(),
        Some(ControllerError::InvalidLease)
    );
    {
        let inner = controller.inner.lock().unwrap();
        assert_eq!(inner.adapter.actions, 0);
        assert!(inner.adapter.capture_wait_until.is_none());
        assert_eq!(inner.last.as_ref().unwrap().revision, Revision(1));
    }

    let valid_lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    let result = controller.act("agent", valid_lease, request()).unwrap();
    assert_eq!(result.effect, ObservedEffect::Changed);
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 1);
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
    assert_eq!(result.receipt.accepted, Some(true));
    assert_eq!(result.native_lock, None);
    assert_eq!(controller.inner.lock().unwrap().adapter.native_queries, 0);

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
    assert_eq!(result.native_lock, None);
    assert_eq!(controller.inner.lock().unwrap().adapter.native_queries, 0);

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

fn lock(
    controller: &Controller<FakeAdapter>,
    key: DeviceKey,
) -> Result<ActionResult, ControllerError> {
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    controller.act(
        "agent",
        lease,
        ActionRequest {
            device: key,
            expected_revision: None,
            action: Action::Lock,
        },
    )
}

#[test]
fn shortcuts_screen_change_does_not_confirm_lock() {
    for native in [LockState::Unlocked, LockState::Uncertain] {
        let (controller, key) = controller();
        controller.inner.lock().unwrap().adapter.native_lock = native;
        let result = lock(&controller, key).unwrap();
        assert_eq!(result.receipt.accepted, Some(true));
        assert_eq!(result.native_lock, Some(native));
        assert_eq!(result.effect, ObservedEffect::Unconfirmed);
        assert_eq!(
            result.after.unwrap().state.foreground_app.as_deref(),
            Some("com.apple.shortcuts")
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(inner.adapter.actions, 1);
        assert_eq!(inner.adapter.native_queries, 1);
    }
}

#[test]
fn native_locked_confirms_postcondition_even_when_capture_fails() {
    for fail_capture in [false, true] {
        let (controller, key) = controller();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.native_lock = LockState::Locked;
            inner.adapter.fail_after_action = fail_capture;
        }
        let result = lock(&controller, key).unwrap();
        assert_eq!(result.receipt.accepted, Some(true));
        assert_eq!(result.native_lock, Some(LockState::Locked));
        assert_eq!(result.effect, ObservedEffect::LockConfirmed);
        assert_eq!(result.after.is_none(), fail_capture);
        let inner = controller.inner.lock().unwrap();
        assert_eq!(inner.adapter.actions, 1);
        assert_eq!(inner.adapter.native_queries, 1);
        assert_eq!(inner.last.is_none(), fail_capture);
    }
}

#[test]
fn lost_lock_ack_preserves_native_proof_without_resending() {
    for native in [LockState::Locked, LockState::Unlocked, LockState::Uncertain] {
        for fail_capture in [false, true] {
            let (controller, key) = controller();
            {
                let mut inner = controller.inner.lock().unwrap();
                inner.adapter.lose_ack = true;
                inner.adapter.native_lock = native;
                inner.adapter.fail_after_action = fail_capture;
            }
            let result = lock(&controller, key).unwrap();
            assert_eq!(result.receipt.accepted, None);
            assert_eq!(result.receipt.action_kind, "lock");
            assert_eq!(result.native_lock, Some(native));
            assert_eq!(
                result.effect,
                if native == LockState::Locked {
                    ObservedEffect::LockConfirmed
                } else {
                    ObservedEffect::Unconfirmed
                }
            );
            assert_eq!(result.after.is_none(), fail_capture);
            let inner = controller.inner.lock().unwrap();
            assert_eq!(inner.adapter.actions, 1);
            assert_eq!(inner.adapter.native_queries, 1);
        }
    }
}

#[test]
fn native_lock_proof_does_not_bypass_identity_checks() {
    for after_action in [false, true] {
        let (controller, key) = controller();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.native_lock = LockState::Locked;
            if after_action {
                inner.adapter.mismatch_after_action = true;
            } else {
                inner.adapter.wda = HardwareId::new("fixture-phone-b").unwrap();
            }
        }
        assert_eq!(
            lock(&controller, key).err(),
            Some(ControllerError::DeviceMismatch)
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(inner.adapter.actions, usize::from(after_action));
        assert_eq!(inner.adapter.native_queries, usize::from(after_action));
        assert!(inner.last.is_none());
    }
}

#[test]
fn non_lock_transport_error_still_fails_without_a_native_query() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    controller.inner.lock().unwrap().adapter.lose_ack = true;
    let result = controller.act(
        "agent",
        lease,
        ActionRequest {
            device: key,
            expected_revision: None,
            action: Action::Home,
        },
    );
    assert_eq!(result.err(), Some(ControllerError::Adapter));
    let inner = controller.inner.lock().unwrap();
    assert_eq!(inner.adapter.actions, 1);
    assert_eq!(inner.adapter.native_queries, 0);
    assert!(inner.last.is_none());
}
