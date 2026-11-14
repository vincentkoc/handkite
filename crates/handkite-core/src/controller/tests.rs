use super::*;
use crate::protocol::{ImageFormat, LockState, Point, ScreenImage};

struct FakeAdapter {
    fail_identity: bool,
    error_diagnostic: Option<AdapterDiagnostic>,
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
    tap_admission: Option<ActionAdmission>,
    leak_home_evidence: bool,
    actions: usize,
    native_lock: LockState,
    native_queries: usize,
    capture_wait_until: Option<Instant>,
    send_deadline: Option<Instant>,
    send_started: Option<Instant>,
    execution_admission: Option<ActionAdmission>,
    after_admission: Option<ActionAdmission>,
}

impl FakeAdapter {
    fn new() -> Self {
        let id = HardwareId::new("fixture-phone-a").unwrap();
        Self {
            fail_identity: false,
            error_diagnostic: None,
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
            tap_admission: None,
            leak_home_evidence: false,
            actions: 0,
            native_lock: LockState::Uncertain,
            native_queries: 0,
            capture_wait_until: None,
            send_deadline: None,
            send_started: None,
            execution_admission: None,
            after_admission: None,
        }
    }
}

impl DeviceAdapter for FakeAdapter {
    fn take_error_diagnostic(&mut self) -> Option<AdapterDiagnostic> {
        self.error_diagnostic.take()
    }

    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        if self.fail_identity {
            return Err(AdapterError);
        }
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
    ) -> Result<(RawObservation, Option<ActionAdmission>), AdapterError> {
        let observation = self.observe()?;
        let admission = if matches!(action, Action::LaunchApp { .. }) || self.leak_home_evidence {
            self.home_launch_until
                .take()
                .map(|until| ActionAdmission::HomeLaunch { until })
                .or_else(|| self.tap_admission.take())
        } else {
            self.tap_admission.take()
        };
        Ok((observation, admission))
    }

    fn observe_after_action(
        &mut self,
        _action: &Action,
        admission: Option<ActionAdmission>,
        _receipt: &TransportReceipt,
    ) -> Result<RawObservation, AdapterError> {
        self.after_admission = admission;
        self.observe()
    }

    fn execute_with_admission(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        admission: Option<ActionAdmission>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execution_admission = admission;
        if matches!(
            admission,
            Some(
                ActionAdmission::NativeHome { .. }
                    | ActionAdmission::NativeModalTap { .. }
                    | ActionAdmission::NativeText { .. }
                    | ActionAdmission::NativeTextGesture { .. }
                    | ActionAdmission::NativeButton { .. }
                    | ActionAdmission::NativeKey { .. }
            )
        ) {
            self.native_queries += 1;
            if self.native_lock != LockState::Unlocked {
                return Err(AdapterError);
            }
        }
        self.execute(action, send_deadline)
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
fn view_change_reports_only_the_components_that_changed() {
    for mask in 1..8 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let changed = ViewChange {
            state: mask & 1 != 0,
            accessibility_tree: mask & 2 != 0,
            screenshot: mask & 4 != 0,
            gesture_admission: None,
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.wda_ready = !changed.state;
            if changed.accessibility_tree {
                inner.adapter.tree_suffix = "changed".into();
            }
            if changed.screenshot {
                inner.adapter.image_suffix = vec![1];
            }
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
                        action: Action::Home,
                    }
                )
                .err(),
            Some(ControllerError::ViewChanged(changed))
        );
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
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
        Some(ControllerError::ViewChanged(ViewChange {
            state: true,
            accessibility_tree: true,
            screenshot: true,
            gesture_admission: None,
        }))
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
fn visible_tap_allows_only_pixel_drift_with_matching_fresh_typed_evidence() {
    let at = Point { x: 20.0, y: 30.0 };
    for change in 0..9 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.tap_admission = Some(ActionAdmission::VisibleTap { at, until });
            match change {
                1 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::VisibleTap {
                        at: Point { x: 21.0, y: 30.0 },
                        until,
                    })
                }
                2 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::VisibleTap {
                        at,
                        until: Instant::now() - Duration::from_secs(1),
                    })
                }
                3 => inner.adapter.tap_admission = Some(ActionAdmission::HomeLaunch { until }),
                4 => inner.adapter.tree_suffix = "changed".into(),
                5 => inner.adapter.wda_ready = false,
                6 => inner.adapter.locked = true,
                7 => inner.adapter.uncertain = true,
                8 => inner.adapter.tap_admission = None,
                _ => {}
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
                action: Action::Tap { at },
            },
        );
        let inner = controller.inner.lock().unwrap();
        if change == 0 {
            let result = result.unwrap();
            assert_eq!(inner.adapter.actions, 1);
            assert!(inner.adapter.send_deadline.unwrap() <= until);
            assert_eq!(result.effect, ObservedEffect::Unconfirmed);
        } else {
            assert!(
                matches!(result, Err(ControllerError::ViewChanged(_))),
                "case {change}"
            );
            assert_eq!(inner.adapter.actions, 0);
        }
    }
}

#[test]
fn native_modal_tap_requires_exact_action_state_tree_and_live_native_unlock() {
    let at = Point { x: 20.0, y: 30.0 };
    for change in 0..11 {
        let (controller, key) = controller();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.app = "com.apple.springboard".into();
            inner.adapter.uncertain = true;
            inner.adapter.native_lock = LockState::Unlocked;
        }
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        let mut action = Action::Tap { at };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.tap_admission = Some(ActionAdmission::NativeModalTap { at, until });
            match change {
                1 => {
                    action = Action::Tap {
                        at: Point { x: 21.0, y: 30.0 },
                    }
                }
                2 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeModalTap {
                        at,
                        until: Instant::now() - Duration::from_secs(1),
                    })
                }
                3 => inner.adapter.tree_suffix = "changed".into(),
                4 => inner.adapter.wda_ready = false,
                5 => inner.adapter.locked = true,
                6 => inner.adapter.uncertain = false,
                7 => inner.adapter.app = "com.example.app".into(),
                8 => inner.adapter.native_lock = LockState::Locked,
                9 => action = Action::Home,
                10 => {
                    action = Action::Text {
                        value: "fixture".into(),
                    }
                }
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        if change == 0 {
            assert!(result.is_ok());
            assert_eq!(inner.adapter.actions, 1);
            assert_eq!(inner.adapter.native_queries, 1);
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        } else {
            assert!(result.is_err(), "case {change}");
            assert_eq!(inner.adapter.actions, 0, "case {change}");
        }
    }
}

#[test]
fn default_adapter_refuses_native_modal_admission_before_execution() {
    struct DefaultAdapter(FakeAdapter);
    impl DeviceAdapter for DefaultAdapter {
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            self.0.identities()
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            self.0.observe()
        }
        fn execute(
            &mut self,
            action: &Action,
            deadline: Instant,
        ) -> Result<TransportReceipt, AdapterError> {
            self.0.execute(action, deadline)
        }
    }
    let mut adapter = DefaultAdapter(FakeAdapter::new());
    let at = Point { x: 20.0, y: 30.0 };
    let until = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        adapter.execute_with_admission(
            &Action::Tap { at },
            until,
            Some(ActionAdmission::NativeModalTap { at, until })
        ),
        Err(AdapterError)
    );
    assert_eq!(adapter.0.actions, 0);
    assert_eq!(
        adapter.execute_with_admission(
            &Action::Text {
                value: "fixture".into()
            },
            until,
            Some(ActionAdmission::NativeText { until })
        ),
        Err(AdapterError)
    );
    assert_eq!(adapter.0.actions, 0);
}

#[test]
fn native_text_allows_only_pixel_drift_with_fresh_unchanged_context() {
    for change in 0..10 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut action = Action::Text {
            value: "fixture".into(),
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = b"cursor".to_vec();
            inner.adapter.tap_admission = Some(ActionAdmission::NativeText { until });
            inner.adapter.native_lock = LockState::Unlocked;
            match change {
                1 => inner.adapter.tree_suffix = "changed".into(),
                2 => inner.adapter.locked = true,
                3 => inner.adapter.uncertain = true,
                4 => inner.adapter.wda_ready = false,
                5 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeText {
                        until: Instant::now(),
                    })
                }
                6 => inner.adapter.native_lock = LockState::Locked,
                7 => action = Action::Home,
                8 => inner.adapter.stale_capture = true,
                9 => {
                    action = Action::Key {
                        key: crate::protocol::KeyboardKey::Backspace,
                    }
                }
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        if change == 0 {
            assert!(result.is_ok());
            assert_eq!(inner.adapter.actions, 1);
            assert_eq!(inner.adapter.native_queries, 1);
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        } else {
            assert!(result.is_err(), "case {change}");
            assert_eq!(inner.adapter.actions, 0, "case {change}");
        }
    }
}

#[test]
fn current_selection_key_is_explicit_and_bound_to_key_and_context() {
    use crate::protocol::KeyboardKey;
    for case in 0..11 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut action = Action::KeyCurrent {
            key: KeyboardKey::Backspace,
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = b"blink".to_vec();
            inner.adapter.tap_admission = Some(ActionAdmission::NativeKey {
                key: KeyboardKey::Backspace,
                until,
            });
            inner.adapter.native_lock = LockState::Unlocked;
            match case {
                1 => {
                    action = Action::Key {
                        key: KeyboardKey::Backspace,
                    }
                }
                2 => {
                    action = Action::KeyCurrent {
                        key: KeyboardKey::Enter,
                    }
                }
                3 => inner.adapter.tree_suffix = "changed-value-or-selection".into(),
                4 => inner.adapter.native_lock = LockState::Locked,
                5 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeKey {
                        key: KeyboardKey::Backspace,
                        until: Instant::now(),
                    })
                }
                6 => inner.adapter.stale_capture = true,
                7 => inner.adapter.wda_ready = false,
                8 => {
                    action = Action::Text {
                        value: "fixture".into(),
                    }
                }
                9 => inner.adapter.app = "different.app".into(),
                10 => {
                    action = Action::KeyCurrent {
                        key: KeyboardKey::Enter,
                    };
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeKey {
                        key: KeyboardKey::Enter,
                        until,
                    });
                }
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(result.is_ok(), matches!(case, 0 | 10), "case {case}");
        assert_eq!(
            inner.adapter.actions,
            usize::from(matches!(case, 0 | 10)),
            "case {case}"
        );
        if matches!(case, 0 | 10) {
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        }
    }
}

#[test]
fn current_selection_text_preserves_context_and_native_admission() {
    for case in 0..9 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let action = Action::TextCurrent {
            value: "Beta".into(),
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = b"blink".to_vec();
            inner.adapter.tap_admission = Some(ActionAdmission::NativeText { until });
            inner.adapter.native_lock = LockState::Unlocked;
            match case {
                1 => inner.adapter.tap_admission = None,
                2 => inner.adapter.tree_suffix = "changed-selection".into(),
                3 => inner.adapter.native_lock = LockState::Locked,
                4 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeText {
                        until: Instant::now(),
                    })
                }
                5 => inner.adapter.stale_capture = true,
                6 => inner.adapter.wda_ready = false,
                7 => inner.adapter.app = "different.app".into(),
                8 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeKey {
                        key: crate::protocol::KeyboardKey::Enter,
                        until,
                    })
                }
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(result.is_ok(), case == 0, "case {case}");
        assert_eq!(inner.adapter.actions, usize::from(case == 0), "case {case}");
        if case == 0 {
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        }
    }
}

#[test]
fn named_button_admission_preserves_tree_state_and_deadline_checks() {
    for case in 0..7 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut action = Action::ClickButton {
            target: crate::protocol::ButtonTarget {
                label: "Close".into(),
                x: 10.0,
                y: 10.0,
                width: 30.0,
                height: 30.0,
            },
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = b"cursor".to_vec();
            inner.adapter.tap_admission = Some(ActionAdmission::NativeButton { until });
            inner.adapter.native_lock = LockState::Unlocked;
            match case {
                1 => inner.adapter.tree_suffix = "changed".into(),
                2 => inner.adapter.locked = true,
                3 => inner.adapter.native_lock = LockState::Locked,
                4 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeButton {
                        until: Instant::now(),
                    })
                }
                5 => inner.adapter.stale_capture = true,
                6 => {
                    action = Action::Tap {
                        at: Point { x: 10.0, y: 10.0 },
                    }
                }
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(result.is_ok(), case == 0, "case {case}");
        assert_eq!(inner.adapter.actions, usize::from(case == 0), "case {case}");
        if case == 0 {
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        }
    }
}

#[test]
fn text_gesture_admission_is_bound_to_point_action_tree_and_native_unlock() {
    let at = Point { x: 20.0, y: 30.0 };
    for case in 0..9 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut action = Action::LongPress {
            at,
            duration_ms: 750,
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = b"cursor".to_vec();
            inner.adapter.tap_admission = Some(ActionAdmission::NativeTextGesture { at, until });
            inner.adapter.native_lock = LockState::Unlocked;
            match case {
                1 => action = Action::DoubleTap { at },
                2 => action = Action::Tap { at },
                3 => {
                    action = Action::DoubleTap {
                        at: Point { x: 21.0, y: 30.0 },
                    }
                }
                4 => inner.adapter.tree_suffix = "changed".into(),
                5 => inner.adapter.native_lock = LockState::Locked,
                6 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeTextGesture {
                        at,
                        until: Instant::now(),
                    })
                }
                7 => inner.adapter.stale_capture = true,
                8 => inner.adapter.tap_admission = None,
                _ => {}
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
                action,
            },
        );
        let inner = controller.inner.lock().unwrap();
        assert_eq!(result.is_ok(), case <= 1, "case {case}");
        assert_eq!(inner.adapter.actions, usize::from(case <= 1));
        let diagnostic = match case {
            3 => Some(GestureAdmission::Mismatched),
            4 => Some(GestureAdmission::Present),
            6 => Some(GestureAdmission::Expired),
            8 => Some(GestureAdmission::Missing),
            _ => None,
        };
        if let Some(expected) = diagnostic {
            let Err(ControllerError::ViewChanged(change)) = result else {
                panic!("case {case} did not report a view change");
            };
            assert_eq!(change.gesture_admission, Some(expected));
        }
    }
}

#[test]
fn visible_tap_evidence_cannot_admit_other_actions() {
    let at = Point { x: 20.0, y: 30.0 };
    for action in [
        Action::Home,
        Action::Lock,
        Action::Text {
            value: "fixture".into(),
        },
        Action::LaunchApp {
            bundle_id: "com.example.app".into(),
        },
        Action::Swipe {
            from: at,
            to: Point { x: 40.0, y: 50.0 },
            duration_ms: 100,
        },
    ] {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.tap_admission = Some(ActionAdmission::VisibleTap {
                at,
                until: Instant::now() + Duration::from_secs(2),
            });
        }
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        assert!(matches!(
            controller.act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action,
                }
            ),
            Err(ControllerError::ViewChanged(_))
        ));
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
}

#[test]
fn native_home_admission_tolerates_only_pixels_and_is_passed_to_execution() {
    for change in 0..10 {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.tap_admission = Some(ActionAdmission::NativeHome { until });
            inner.adapter.native_lock = LockState::Unlocked;
            match change {
                1 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::NativeHome {
                        until: Instant::now() - Duration::from_secs(1),
                    })
                }
                2 => {
                    inner.adapter.tap_admission = Some(ActionAdmission::VisibleTap {
                        at: Point { x: 20.0, y: 30.0 },
                        until,
                    })
                }
                3 => inner.adapter.tap_admission = Some(ActionAdmission::HomeLaunch { until }),
                4 => inner.adapter.tree_suffix = "changed".into(),
                5 => inner.adapter.wda_ready = false,
                6 => inner.adapter.locked = true,
                7 => inner.adapter.uncertain = true,
                8 => inner.adapter.tap_admission = None,
                9 => inner.adapter.native_lock = LockState::Locked,
                _ => {}
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
                action: Action::Home,
            },
        );
        let inner = controller.inner.lock().unwrap();
        if change == 0 {
            assert!(result.is_ok());
            assert_eq!(inner.adapter.actions, 1);
            assert_eq!(
                inner.adapter.execution_admission,
                Some(ActionAdmission::NativeHome { until })
            );
            assert_eq!(inner.adapter.native_queries, 1);
            assert!(inner.adapter.send_deadline.unwrap() <= until);
        } else {
            assert!(
                matches!(
                    result,
                    Err(ControllerError::ViewChanged(_)) | Err(ControllerError::Adapter)
                ),
                "case {change}"
            );
            assert_eq!(inner.adapter.actions, 0);
        }
    }
}

#[test]
fn home_completion_keeps_acknowledgment_when_observation_fails() {
    for fail in [false, true] {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        let admission = ActionAdmission::NativeHome {
            until: Instant::now() + Duration::from_secs(2),
        };
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.tap_admission = Some(admission);
            inner.adapter.native_lock = LockState::Unlocked;
            inner.adapter.fail_after_action = fail;
        }
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        let result = controller
            .act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::Home,
                },
            )
            .unwrap();
        assert_eq!(result.receipt.accepted, Some(true));
        assert_eq!(result.after.is_none(), fail);
        let inner = controller.inner.lock().unwrap();
        assert_eq!(inner.adapter.actions, 1);
        assert_eq!(inner.adapter.after_admission, Some(admission));
        if fail {
            assert_eq!(result.effect, ObservedEffect::Unconfirmed);
            assert!(inner.last.is_none());
        }
    }
}

#[test]
fn native_home_evidence_cannot_admit_other_actions() {
    let at = Point { x: 20.0, y: 30.0 };
    for action in [
        Action::Tap { at },
        Action::Lock,
        Action::Text {
            value: "fixture".into(),
        },
        Action::LaunchApp {
            bundle_id: "com.example.app".into(),
        },
        Action::Swipe {
            from: at,
            to: Point { x: 40.0, y: 50.0 },
            duration_ms: 100,
        },
    ] {
        let (controller, key) = controller();
        let before = controller.observe(&key).unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.image_suffix = vec![1];
            inner.adapter.tap_admission = Some(ActionAdmission::NativeHome {
                until: Instant::now() + Duration::from_secs(2),
            });
        }
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        assert!(matches!(
            controller.act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action
                }
            ),
            Err(ControllerError::ViewChanged(_))
        ));
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 0);
    }
}

#[test]
fn exact_view_home_from_locked_does_not_require_unlock_admission() {
    let (controller, key) = controller();
    controller.inner.lock().unwrap().adapter.locked = true;
    let before = controller.observe(&key).unwrap();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    assert!(
        controller
            .act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::Home
                }
            )
            .is_ok()
    );
    let inner = controller.inner.lock().unwrap();
    assert_eq!(inner.adapter.actions, 1);
    assert_eq!(inner.adapter.native_queries, 0);
    assert_eq!(inner.adapter.execution_admission, None);
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
                assert_eq!(
                    result.err(),
                    Some(ControllerError::ViewChanged(ViewChange {
                        state: false,
                        accessibility_tree: tree,
                        screenshot: image,
                        gesture_admission: None,
                    }))
                );
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
        assert_eq!(
            result.err(),
            Some(ControllerError::ViewChanged(ViewChange {
                state: change < 3,
                accessibility_tree: true,
                screenshot: change == 0,
                gesture_admission: None,
            }))
        );
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
            Some(ControllerError::ViewChanged(ViewChange {
                state: false,
                accessibility_tree: true,
                screenshot: true,
                gesture_admission: None,
            }))
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
fn identity_diagnostic_is_consumed_before_capture_or_input() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    {
        let mut inner = controller.inner.lock().unwrap();
        inner.adapter.fail_identity = true;
        inner.adapter.error_diagnostic = Some(AdapterDiagnostic::IdentityCheck);
    }
    assert_eq!(
        controller
            .act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: None,
                    action: Action::Home,
                }
            )
            .err(),
        Some(ControllerError::AdapterAt(AdapterDiagnostic::IdentityCheck))
    );
    let inner = controller.inner.lock().unwrap();
    assert_eq!(inner.adapter.actions, 0);
    assert_eq!(inner.adapter.error_diagnostic, None);
}

#[test]
fn adapter_diagnostic_is_consumed_at_failure_without_replay() {
    for diagnostic in AdapterDiagnostic::ALL {
        let (controller, key) = controller();
        let lease = controller
            .acquire("agent".into(), Duration::from_secs(30))
            .unwrap();
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.adapter.lose_ack = true;
            inner.adapter.error_diagnostic = Some(diagnostic);
        }
        let action = || ActionRequest {
            device: key.clone(),
            expected_revision: None,
            action: Action::Home,
        };
        assert_eq!(
            controller.act("agent", lease, action()).err(),
            Some(ControllerError::AdapterAt(diagnostic))
        );
        {
            let inner = controller.inner.lock().unwrap();
            assert_eq!(inner.adapter.actions, 1);
            assert_eq!(inner.adapter.error_diagnostic, None);
            assert!(inner.last.is_none());
        }
        assert_eq!(
            controller.act("agent", lease, action()).err(),
            Some(ControllerError::Adapter)
        );
        assert_eq!(controller.inner.lock().unwrap().adapter.actions, 2);
    }
}

#[test]
fn preaction_capture_diagnostic_does_not_dispatch() {
    let (controller, key) = controller();
    let lease = controller
        .acquire("agent".into(), Duration::from_secs(30))
        .unwrap();
    {
        let mut inner = controller.inner.lock().unwrap();
        inner.adapter.fail_capture = true;
        inner.adapter.error_diagnostic = Some(AdapterDiagnostic::PreActionCapture);
    }
    assert_eq!(
        controller
            .act(
                "agent",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: None,
                    action: Action::Home,
                }
            )
            .err(),
        Some(ControllerError::AdapterAt(
            AdapterDiagnostic::PreActionCapture
        ))
    );
    let inner = controller.inner.lock().unwrap();
    assert_eq!(inner.adapter.actions, 0);
    assert_eq!(inner.adapter.error_diagnostic, None);
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
