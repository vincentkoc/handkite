use super::*;
use crate::protocol::{ImageFormat, LockState, Point, ScreenImage};

struct FakeAdapter {
    usb: HardwareId,
    wda: HardwareId,
    app: String,
    tree_suffix: String,
    image_suffix: Vec<u8>,
    wda_ready: bool,
    stale_capture: bool,
    fail_capture: bool,
    fail_after_action: bool,
    lose_ack: bool,
    mismatch_after_action: bool,
    locked: bool,
    uncertain: bool,
    home_launch_until: Option<Instant>,
    leak_home_evidence: bool,
    actions: usize,
    native_lock: LockState,
    native_queries: usize,
    capture_wait_until: Option<Instant>,
    send_deadline: Option<Instant>,
    send_started: Option<Instant>,
}

impl FakeAdapter {
    fn new() -> Self {
        let id = HardwareId::new("fixture-phone-a").unwrap();
        Self {
            usb: id.clone(),
            wda: id,
            app: "com.example.notes".into(),
            tree_suffix: String::new(),
            image_suffix: Vec::new(),
            wda_ready: true,
            stale_capture: false,
            fail_capture: false,
            fail_after_action: false,
            lose_ack: false,
            mismatch_after_action: false,
            locked: false,
            uncertain: false,
            home_launch_until: None,
            leak_home_evidence: false,
            actions: 0,
            native_lock: LockState::Uncertain,
            native_queries: 0,
            capture_wait_until: None,
            send_deadline: None,
            send_started: None,
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
                } else if self.uncertain {
                    LockState::Uncertain
                } else {
                    LockState::Unlocked
                },
                wda_ready: self.wda_ready,
                foreground_app: Some(self.app.clone()),
            },
            accessibility_tree: format!("<app name=\"{}\" />{}", self.app, self.tree_suffix),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: [self.app.as_bytes(), self.image_suffix.as_slice()].concat(),
            },
        })
    }

    fn observe_for_action(
        &mut self,
        action: &Action,
    ) -> Result<(RawObservation, Option<Instant>), AdapterError> {
        let observation = self.observe()?;
        let admission = if matches!(action, Action::LaunchApp { .. }) || self.leak_home_evidence {
            self.home_launch_until.take()
        } else {
            None
        };
        Ok((observation, admission))
    }

    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        if Instant::now() >= send_deadline {
            return Err(AdapterError);
        }
        self.send_started = Some(Instant::now());
        self.send_deadline = Some(send_deadline);
        self.actions += 1;
        if self.fail_after_action {
            self.fail_capture = true;
        }
        if matches!(action, Action::Home) {
            self.app = "com.apple.springboard".into();
        }
        if let Action::LaunchApp { bundle_id } = action {
            self.app = bundle_id.clone();
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

#[test]
fn action_send_deadline_is_bounded_by_lease_and_observation() {
    let (first_controller, key) = controller();
    let lease = first_controller
        .acquire("agent".into(), Duration::from_secs(2))
        .unwrap();
    let lease_expires = first_controller
        .inner
        .lock()
        .unwrap()
        .lease
        .as_ref()
        .unwrap()
        .expires_at;
    first_controller
        .act(
            "agent",
            lease,
            ActionRequest {
                device: key.clone(),
                expected_revision: None,
                action: Action::Home,
            },
        )
        .unwrap();
    assert!(
        first_controller
            .inner
            .lock()
            .unwrap()
            .adapter
            .send_deadline
            .unwrap()
            <= lease_expires
    );

    let (mut controller, key) = controller();
    controller.max_age_ms = 1_000;
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    controller
        .act(
            "agent",
            lease,
            ActionRequest {
                device: key,
                expected_revision: None,
                action: Action::Home,
            },
        )
        .unwrap();
    let inner = controller.inner.lock().unwrap();
    assert!(
        inner.adapter.send_deadline.unwrap()
            <= inner.adapter.send_started.unwrap() + Duration::from_secs(1)
    );
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
        Some(ControllerError::ViewChanged)
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

#[test]
fn home_launch_tolerates_visual_drift_only_with_fresh_admission() {
    for (tree, image) in [(true, false), (false, true), (true, true)] {
        for admission in [true, false] {
            let (controller, key) = controller();
            {
                let mut inner = controller.inner.lock().unwrap();
                inner.adapter.app = "com.apple.springboard".into();
                inner.adapter.uncertain = true;
            }
            let before = controller.observe(&key).unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            {
                let mut inner = controller.inner.lock().unwrap();
                if tree {
                    inner.adapter.tree_suffix = "animation".into();
                }
                if image {
                    inner.adapter.image_suffix = vec![1];
                }
                inner.adapter.home_launch_until = admission.then_some(until);
            }
            let lease = controller
                .acquire("agent".into(), Duration::from_secs(30))
                .unwrap();
            let result = controller.act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::LaunchApp {
                        bundle_id: "com.apple.mobiletimer".into(),
                    },
                },
            );
            let inner = controller.inner.lock().unwrap();
            if admission {
                assert!(result.is_ok());
                assert_eq!(inner.adapter.actions, 1);
                assert!(inner.adapter.send_deadline.unwrap() <= until);
            } else {
                assert_eq!(result.err(), Some(ControllerError::ViewChanged));
                assert_eq!(inner.adapter.actions, 0);
            }
        }
    }
}

#[test]
fn home_launch_rejects_changed_state_or_expired_admission() {
    for change in 0..4 {
        let (controller, key) = controller();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.app = "com.apple.springboard".into();
            inner.adapter.uncertain = true;
        }
        let before = controller.observe(&key).unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.tree_suffix = "animation".into();
            inner.adapter.home_launch_until = Some(Instant::now() + Duration::from_secs(2));
            match change {
                0 => inner.adapter.app = "com.example.other".into(),
                1 => inner.adapter.locked = true,
                2 => inner.adapter.wda_ready = false,
                _ => {
                    inner.adapter.home_launch_until = Some(Instant::now() - Duration::from_secs(1))
                }
            }
        }
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        let result = controller.act(
            "agent",
            lease,
            ActionRequest {
                device: key,
                expected_revision: Some(before.revision),
                action: Action::LaunchApp {
                    bundle_id: "com.apple.mobiletimer".into(),
                },
            },
        );
        assert_eq!(result.err(), Some(ControllerError::ViewChanged));
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
}

#[test]
fn visual_drift_still_blocks_other_actions() {
    for action in [
        Action::Home,
        Action::Lock,
        Action::Tap {
            at: Point { x: 20.0, y: 30.0 },
        },
        Action::Text {
            value: "fixture".into(),
        },
        Action::Swipe {
            from: Point { x: 20.0, y: 30.0 },
            to: Point { x: 40.0, y: 50.0 },
            duration_ms: 100,
        },
    ] {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.tree_suffix = "animation".into();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.leak_home_evidence = true;
            inner.adapter.home_launch_until = Some(Instant::now() + Duration::from_secs(2));
        }
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            controller
                .act(
                    "agent",
                    lease,
                    ActionRequest {
                        device: key,
                        expected_revision: Some(before.revision),
                        action,
                    }
                )
                .err(),
            Some(ControllerError::ViewChanged)
        );
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
}

#[test]
fn fresh_home_evidence_allows_only_one_launch_from_uncertain_lock() {
    let (controller, key) = controller();
    {
        let mut inner = controller.inner.lock().unwrap();
        inner.adapter.app = "com.apple.springboard".into();
        inner.adapter.uncertain = true;
    }
    let before = controller.observe(&key).unwrap();
    assert_eq!(before.state.lock, LockState::Uncertain);
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    let evidence_until = Instant::now() + Duration::from_secs(2);
    controller.inner.lock().unwrap().adapter.home_launch_until = Some(evidence_until);
    let result = controller
        .act(
            "agent",
            lease,
            ActionRequest {
                device: key.clone(),
                expected_revision: Some(before.revision),
                action: Action::LaunchApp {
                    bundle_id: "com.apple.mobiletimer".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(result.effect, ObservedEffect::Changed);
    assert_eq!(result.after.unwrap().state.lock, LockState::Uncertain);
    let inner = controller.inner.lock().unwrap();
    assert_eq!(inner.adapter.actions, 1);
    assert!(inner.adapter.send_deadline.unwrap() <= evidence_until);
    drop(inner);
    assert_eq!(
        controller
            .act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: None,
                    action: Action::LaunchApp {
                        bundle_id: "com.example.other".into(),
                    },
                },
            )
            .err(),
        Some(ControllerError::LockedAction)
    );
    assert_eq!(controller.inner.lock().unwrap().adapter.actions, 1);
}

#[test]
fn home_evidence_cannot_authorize_locked_or_non_launch_actions() {
    let denied = [
        Action::Tap {
            at: Point { x: 20.0, y: 30.0 },
        },
        Action::Swipe {
            from: Point { x: 20.0, y: 30.0 },
            to: Point { x: 40.0, y: 50.0 },
            duration_ms: 100,
        },
        Action::Text {
            value: "fixture".into(),
        },
        Action::Lock,
    ];
    for action in denied {
        let (controller, key) = controller();
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.uncertain = true;
            inner.adapter.home_launch_until = Some(Instant::now() + Duration::from_secs(2));
            inner.adapter.leak_home_evidence = true;
        }
        assert_eq!(
            controller
                .act(
                    "agent",
                    lease,
                    ActionRequest {
                        device: key,
                        expected_revision: None,
                        action,
                    },
                )
                .err(),
            Some(ControllerError::LockedAction)
        );
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
    for (locked, deadline) in [
        (false, None),
        (false, Some(Instant::now() - Duration::from_millis(1))),
        (true, Some(Instant::now() + Duration::from_secs(2))),
    ] {
        let (controller, key) = controller();
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.uncertain = true;
            inner.adapter.locked = locked;
            inner.adapter.home_launch_until = deadline;
        }
        assert_eq!(
            controller
                .act(
                    "agent",
                    lease,
                    ActionRequest {
                        device: key,
                        expected_revision: None,
                        action: Action::LaunchApp {
                            bundle_id: "com.apple.mobiletimer".into(),
                        },
                    },
                )
                .err(),
            Some(ControllerError::LockedAction)
        );
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
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
