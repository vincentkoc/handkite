use super::*;

/// Current field/context evidence, deliberately not a full tree or a revision.
/// The adapter retains the matching field identity, value, action and session.
pub struct PreparedText {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub token: u64,
    pub valid_until: Instant,
}

pub struct TextReadback {
    pub image: RawImageObservation,
    /// The same public field changed by the requested insertion/replacement.
    /// App, session and ownership must still match. No caret is inferred.
    pub input_verified: bool,
}

impl<A: DeviceAdapter> Controller<A> {
    pub(super) fn try_focused_text(
        &self,
        inner: &mut Inner<A>,
        request: &ActionRequest,
        capture: CaptureMode,
    ) -> Result<Option<ActionResult>, ControllerError> {
        if capture != CaptureMode::ImageOnly
            || request.expected_revision.is_none()
            || !matches!(&request.action, Action::TextCurrent { .. })
        {
            return Ok(None);
        }
        let Some(baseline) = &inner.last else {
            return Ok(None);
        };
        if self.validate_window(&baseline.window).is_err() || !public_text_state(&baseline.state) {
            return Ok(None);
        }
        let baseline_state = baseline.state.clone();
        let prepared = inner
            .adapter
            .prepare_current_text(&request.action, baseline)
            .map_err(|_| Self::adapter_error(inner))?;
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        self.binding
            .verify(
                &request.device,
                &prepared.connected_usb,
                &prepared.wda_target,
            )
            .map_err(|_| ControllerError::DeviceMismatch)?;
        self.validate_window(&prepared.window)?;
        if !public_text_state(&prepared.state) {
            inner.last = None;
            return Err(ControllerError::LockedAction);
        }
        if prepared.state != baseline_state {
            inner.last = None;
            return Err(ControllerError::StaleObservation);
        }
        let lease = inner
            .lease
            .as_ref()
            .filter(|lease| lease.expires_at > Instant::now())
            .ok_or(ControllerError::InvalidLease)?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::StaleObservation)?
            .as_millis() as u64;
        let age = now_ms
            .checked_sub(prepared.window.finished_unix_ms)
            .ok_or(ControllerError::StaleObservation)?;
        let remaining = self
            .max_age_ms
            .checked_sub(age)
            .filter(|remaining| *remaining > 0)
            .ok_or(ControllerError::StaleObservation)?;
        let send_deadline = Instant::now()
            .checked_add(Duration::from_millis(remaining))
            .ok_or(ControllerError::StaleObservation)?
            .min(lease.expires_at)
            .min(prepared.valid_until);
        if send_deadline <= Instant::now() {
            return Err(ControllerError::StaleObservation);
        }

        // Image-only output cannot supply an actionable full-view baseline.
        // Invalidate before dispatch, including when delivery becomes unknown.
        inner.last = None;
        let receipt = inner
            .adapter
            .execute_prepared_text(&request.action, prepared.token, send_deadline)
            .map_err(|_| Self::adapter_error(inner))?;
        let observed = inner.adapter.observe_after_prepared_text(prepared.token);
        let (after_image, verified) = match observed {
            Ok(readback) => match self.accept_image(&request.device, readback.image) {
                Ok(image) => (Some(image), readback.input_verified),
                Err(ControllerError::DeviceMismatch) => {
                    return Err(ControllerError::DeviceMismatch);
                }
                Err(_) => (None, false),
            },
            Err(_) => (None, false),
        };
        Ok(Some(ActionResult {
            effect: if receipt.accepted == Some(true) && verified {
                ObservedEffect::Changed
            } else {
                ObservedEffect::Unconfirmed
            },
            receipt,
            after: None,
            after_image,
            native_lock: None,
        }))
    }
}

fn public_text_state(state: &DeviceState) -> bool {
    state.lock == LockState::Unlocked
        && state.wda_ready
        && state
            .foreground_app
            .as_deref()
            .is_some_and(|app| !app.is_empty() && app != "com.apple.springboard")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ImageFormat;

    #[derive(Default)]
    struct Adapter {
        prepare: u8,
        sends: usize,
        captures: usize,
        verified: bool,
        fail_readback: bool,
    }

    fn raw() -> RawObservation {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        RawObservation {
            connected_usb: HardwareId::new("fixture-device").unwrap(),
            wda_target: HardwareId::new("fixture-device").unwrap(),
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some("example.editor".into()),
            },
            accessibility_tree: "<editor/>".into(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![1],
            },
        }
    }

    impl DeviceAdapter for Adapter {
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            let raw = raw();
            Ok((raw.connected_usb, raw.wda_target))
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            self.captures += 1;
            Ok(raw())
        }
        fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
            panic!("narrow input must use its bound token")
        }
        fn prepare_current_text(
            &mut self,
            _: &Action,
            _: &Observation,
        ) -> Result<Option<PreparedText>, AdapterError> {
            let mut raw = raw();
            if self.prepare == 1 {
                raw.state.lock = LockState::Locked;
            }
            if self.prepare == 2 {
                raw.state.foreground_app = Some("example.other".into());
            }
            if self.prepare == 3 {
                raw.wda_target = HardwareId::new("wrong-device").unwrap();
            }
            Ok(Some(PreparedText {
                connected_usb: raw.connected_usb,
                wda_target: raw.wda_target,
                state: raw.state,
                window: raw.window,
                token: 7,
                valid_until: if self.prepare == 4 {
                    Instant::now() - Duration::from_secs(1)
                } else {
                    Instant::now() + Duration::from_secs(1)
                },
            }))
        }
        fn execute_prepared_text(
            &mut self,
            _: &Action,
            token: u64,
            deadline: Instant,
        ) -> Result<TransportReceipt, AdapterError> {
            assert_eq!(token, 7);
            assert!(deadline > Instant::now());
            self.sends += 1;
            Ok(TransportReceipt {
                accepted: Some(true),
                action_kind: "text".into(),
            })
        }
        fn observe_after_prepared_text(
            &mut self,
            token: u64,
        ) -> Result<TextReadback, AdapterError> {
            assert_eq!(token, 7);
            if self.fail_readback {
                return Err(AdapterError);
            }
            let raw = raw();
            Ok(TextReadback {
                image: RawImageObservation {
                    connected_usb: raw.connected_usb,
                    wda_target: raw.wda_target,
                    window: raw.window,
                    screenshot: raw.screenshot,
                },
                input_verified: self.verified,
            })
        }
    }

    fn fixture(adapter: Adapter) -> (Controller<Adapter>, DeviceKey, LeaseId, ActionRequest) {
        let key = DeviceKey::new("fixture-phone").unwrap();
        let controller = Controller::new(
            DeviceBinding {
                key: key.clone(),
                usb_id: raw().connected_usb,
            },
            adapter,
            1000,
            5000,
        );
        let lease = controller
            .acquire("owner".into(), Duration::from_secs(10))
            .unwrap();
        let observation = controller.observe(&key).unwrap();
        let request = ActionRequest {
            device: key.clone(),
            expected_revision: Some(observation.revision),
            action: Action::TextCurrent { value: "X".into() },
        };
        (controller, key, lease, request)
    }

    #[test]
    fn fresh_bound_text_uses_no_full_recapture_and_invalidates_revision() {
        let (controller, _, lease, request) = fixture(Adapter {
            verified: true,
            ..Adapter::default()
        });
        let result = controller
            .act_with_capture("owner", lease, request.clone(), CaptureMode::ImageOnly)
            .unwrap();
        assert!(result.effect == ObservedEffect::Changed);
        assert!(result.after.is_none() && result.after_image.is_some());
        assert!(matches!(
            controller.act_with_capture("owner", lease, request, CaptureMode::ImageOnly),
            Err(ControllerError::StaleRevision)
        ));
        let inner = controller.inner.lock().unwrap();
        assert_eq!((inner.adapter.captures, inner.adapter.sends), (1, 1));
    }

    #[test]
    fn unsafe_or_expired_preparation_never_sends() {
        for prepare in 1..=4 {
            let (controller, _, lease, request) = fixture(Adapter {
                prepare,
                ..Adapter::default()
            });
            assert!(
                controller
                    .act_with_capture("owner", lease, request, CaptureMode::ImageOnly)
                    .is_err()
            );
            assert_eq!(controller.inner.lock().unwrap().adapter.sends, 0);
        }
    }

    #[test]
    fn failed_or_negative_readback_keeps_ack_and_never_replays() {
        for fail_readback in [false, true] {
            let (controller, _, lease, request) = fixture(Adapter {
                fail_readback,
                ..Adapter::default()
            });
            let result = controller
                .act_with_capture("owner", lease, request, CaptureMode::ImageOnly)
                .unwrap();
            assert!(result.effect == ObservedEffect::Unconfirmed);
            assert_eq!(result.receipt.accepted, Some(true));
            assert_eq!(controller.inner.lock().unwrap().adapter.sends, 1);
        }
    }
}
