//! Alert authority is disjoint from ordinary, target and navigation revisions.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertButton {
    pub target_id: String,
    pub label: String,
    pub rect: [f64; 4],
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertDescriptor {
    pub alert_id: String,
    pub class: String,
    pub title: String,
    pub body: String,
    pub buttons: Vec<AlertButton>,
}
#[derive(Clone)]
pub struct AlertObservation {
    pub observation: Observation,
    pub alert: AlertDescriptor,
}
pub struct RawAlertObservation {
    pub observation: RawObservation,
    pub alert: AlertDescriptor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRequest {
    pub alert_id: String,
    pub target_id: String,
}
pub struct AlertCompletion {
    pub image: RawImageObservation,
    pub dismissed: bool,
}
fn handle(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|tail| {
        tail.len() == 32
            && tail
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}
impl AlertRequest {
    pub fn valid(&self) -> bool {
        handle(&self.alert_id, "a_") && handle(&self.target_id, "ab_")
    }
}
impl AlertDescriptor {
    pub fn valid(&self) -> bool {
        handle(&self.alert_id, "a_")
            && self.class == "notification_permission_en_v1"
            && !self.title.is_empty()
            && self.title.len() <= 1024
            && self.body.len() <= 2048
            && self.buttons.len() == 2
            && self.buttons.iter().all(|b| {
                handle(&b.target_id, "ab_")
                    && !b.label.is_empty()
                    && b.rect.iter().all(|n| n.is_finite())
                    && b.rect[0] >= 0.0
                    && b.rect[1] >= 0.0
                    && b.rect[2] > 0.0
                    && b.rect[3] > 0.0
            })
            && self.buttons[0].target_id != self.buttons[1].target_id
    }
}
fn alert_state(state: &DeviceState) -> bool {
    state.lock == LockState::Unlocked
        && state.wda_ready
        && state.foreground_app.as_deref() == Some("com.apple.springboard")
}
impl<A: DeviceAdapter> Controller<A> {
    pub fn observe_alert(&self, device: &DeviceKey) -> Result<AlertObservation, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        inner.alert_last = None;
        inner
            .adapter
            .settle_target_effects()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let raw = inner
            .adapter
            .observe_alert()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        if !alert_state(&raw.observation.state) || !raw.alert.valid() {
            return Err(ControllerError::LockedAction);
        }
        let observed = AlertObservation {
            observation: self.accept_capture(&mut inner, device, raw.observation)?,
            alert: raw.alert,
        };
        inner.alert_last = Some(observed.clone());
        Ok(observed)
    }

    pub fn tap_alert(
        &self,
        owner: &str,
        lease_id: LeaseId,
        device: &DeviceKey,
        revision: Revision,
        request: AlertRequest,
    ) -> Result<ActionResult, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&inner.lease, Some(l) if l.id == lease_id && l.owner == owner && l.expires_at > Instant::now())
        {
            return Err(ControllerError::InvalidLease);
        }
        if device != &self.binding.key {
            return Err(ControllerError::DeviceMismatch);
        }
        // Every admitted attempt consumes authority, including a refused preparation.
        let baseline = inner
            .alert_last
            .take()
            .ok_or(ControllerError::StaleRevision)?;
        if !request.valid()
            || revision != inner.revision
            || revision != baseline.observation.revision
            || request.alert_id != baseline.alert.alert_id
            || !baseline
                .alert
                .buttons
                .iter()
                .any(|b| b.target_id == request.target_id)
        {
            return Err(ControllerError::StaleRevision);
        }
        self.validate_window(&baseline.observation.window)?;
        if !alert_state(&baseline.observation.state) {
            return Err(ControllerError::LockedAction);
        }
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        inner
            .adapter
            .settle_target_effects()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let prepared = inner
            .adapter
            .prepare_alert(&request, &baseline)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        self.binding
            .verify(device, &prepared.connected_usb, &prepared.wda_target)
            .map_err(|_| ControllerError::DeviceMismatch)?;
        self.validate_window(&prepared.window)?;
        self.validate_window(&baseline.observation.window)?;
        if prepared.state != baseline.observation.state || !alert_state(&prepared.state) {
            return Err(ControllerError::StaleObservation);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::StaleObservation)?
            .as_millis() as u64;
        let remaining = self
            .max_age_ms
            .checked_sub(
                now.checked_sub(baseline.observation.window.finished_unix_ms)
                    .ok_or(ControllerError::StaleObservation)?,
            )
            .filter(|n| *n > 0)
            .ok_or(ControllerError::StaleObservation)?;
        let lease = inner
            .lease
            .as_ref()
            .filter(|l| l.expires_at > Instant::now())
            .ok_or(ControllerError::InvalidLease)?;
        let deadline = (Instant::now() + Duration::from_millis(remaining))
            .min(lease.expires_at)
            .min(prepared.valid_until);
        if deadline <= Instant::now() {
            return Err(ControllerError::StaleObservation);
        }
        let receipt = inner
            .adapter
            .execute_alert(prepared.token, deadline)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let after = inner
            .adapter
            .complete_alert(prepared.token)
            .ok()
            .and_then(|read| {
                self.accept_image(device, read.image)
                    .ok()
                    .map(|image| (image, read.dismissed))
            });
        let verified = receipt.accepted == Some(true)
            && receipt.action_kind == "tap_alert"
            && after.as_ref().is_some_and(|(_, dismissed)| *dismissed);
        let result = ActionResult {
            receipt,
            effect: if verified {
                ObservedEffect::Changed
            } else {
                ObservedEffect::Unconfirmed
            },
            after: None,
            after_image: after.map(|(image, _)| image),
            native_lock: None,
        };
        renew_verified_action(&mut inner.lease, owner, lease_id, &result, Instant::now());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ALERT: &str = "a_11111111111111111111111111111111";
    const BUTTON: &str = "ab_22222222222222222222222222222222";
    #[derive(Default)]
    struct Adapter {
        sends: usize,
        fail: u8,
        token: u64,
    }
    fn raw() -> RawObservation {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        RawObservation {
            connected_usb: HardwareId::new("fixture").unwrap(),
            wda_target: HardwareId::new("fixture").unwrap(),
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some("com.apple.springboard".into()),
            },
            accessibility_tree: String::new(),
            screenshot: ScreenImage {
                format: crate::protocol::ImageFormat::Png,
                bytes: vec![1],
            },
        }
    }
    fn descriptor() -> AlertDescriptor {
        AlertDescriptor {
            alert_id: ALERT.into(),
            class: "notification_permission_en_v1".into(),
            title: "Fixture".into(),
            body: "Notifications".into(),
            buttons: vec![
                AlertButton {
                    target_id: BUTTON.into(),
                    label: "Don't Allow".into(),
                    rect: [1., 1., 10., 10.],
                },
                AlertButton {
                    target_id: "ab_33333333333333333333333333333333".into(),
                    label: "Allow".into(),
                    rect: [11., 1., 10., 10.],
                },
            ],
        }
    }
    impl DeviceAdapter for Adapter {
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            Ok((raw().connected_usb, raw().wda_target))
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            Ok(raw())
        }
        fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
            self.sends += 1;
            Err(AdapterError)
        }
        fn observe_alert(&mut self) -> Result<RawAlertObservation, AdapterError> {
            Ok(RawAlertObservation {
                observation: raw(),
                alert: descriptor(),
            })
        }
        fn prepare_alert(
            &mut self,
            _: &AlertRequest,
            _b: &AlertObservation,
        ) -> Result<PreparedText, AdapterError> {
            if self.fail == 1 {
                return Err(AdapterError);
            }
            self.token += 1;
            let r = raw();
            Ok(PreparedText {
                token: self.token,
                connected_usb: r.connected_usb,
                wda_target: r.wda_target,
                window: r.window,
                state: if self.fail == 2 {
                    DeviceState {
                        lock: LockState::Locked,
                        ..r.state
                    }
                } else {
                    r.state
                },
                valid_until: Instant::now() + Duration::from_secs(1),
            })
        }
        fn execute_alert(&mut self, _: u64, _: Instant) -> Result<TransportReceipt, AdapterError> {
            self.sends += 1;
            if self.fail == 3 {
                Err(AdapterError)
            } else {
                Ok(TransportReceipt {
                    action_kind: "tap_alert".into(),
                    accepted: Some(true),
                })
            }
        }
        fn complete_alert(&mut self, _: u64) -> Result<AlertCompletion, AdapterError> {
            let r = raw();
            Ok(AlertCompletion {
                image: RawImageObservation {
                    connected_usb: r.connected_usb,
                    wda_target: r.wda_target,
                    window: r.window,
                    screenshot: r.screenshot,
                },
                dismissed: self.fail != 4,
            })
        }
    }
    fn setup(fail: u8) -> (Controller<Adapter>, DeviceKey, LeaseId) {
        let key = DeviceKey::new("fixture").unwrap();
        let id = HardwareId::new("fixture").unwrap();
        let c = Controller::new(
            DeviceBinding {
                key: key.clone(),
                usb_id: id,
            },
            Adapter {
                fail,
                ..Default::default()
            },
            5000,
            5000,
        );
        let l = c.acquire("owner".into(), Duration::from_secs(120)).unwrap();
        (c, key, l)
    }
    fn request() -> AlertRequest {
        AlertRequest {
            alert_id: ALERT.into(),
            target_id: BUTTON.into(),
        }
    }
    #[test]
    fn alert_authority_is_one_use_even_on_preparation_or_delivery_failure() {
        for fail in 0..=4 {
            let (c, k, l) = setup(fail);
            let b = c.observe_alert(&k).unwrap();
            let result = c.tap_alert("owner", l, &k, b.observation.revision, request());
            if fail == 0 {
                assert_eq!(result.as_ref().unwrap().effect, ObservedEffect::Changed);
            }
            if fail == 4 {
                assert_eq!(result.as_ref().unwrap().effect, ObservedEffect::Unconfirmed);
            }
            assert!(
                c.tap_alert("owner", l, &k, b.observation.revision, request())
                    .is_err()
            );
            assert_eq!(
                c.inner.lock().unwrap().adapter.sends,
                usize::from(!matches!(fail, 1 | 2))
            );
        }
    }
    #[test]
    fn wrong_owner_revision_target_and_normal_capture_refuse_without_input() {
        for case in 0..4 {
            let (c, k, l) = setup(0);
            let b = c.observe_alert(&k).unwrap();
            let mut r = request();
            if case == 2 {
                r.target_id = "ab_44444444444444444444444444444444".into();
            }
            if case == 3 {
                c.observe(&k).unwrap();
            }
            assert!(
                c.tap_alert(
                    if case == 0 { "other" } else { "owner" },
                    l,
                    &k,
                    if case == 1 {
                        Revision(99)
                    } else {
                        b.observation.revision
                    },
                    r
                )
                .is_err()
            );
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
        }
    }
    #[test]
    fn expiry_device_mismatch_and_cross_scope_calls_never_dispatch() {
        for case in 0..4 {
            let (c, k, l) = setup(0);
            let b = c.observe_alert(&k).unwrap();
            if case == 0 {
                c.inner.lock().unwrap().lease.as_mut().unwrap().expires_at =
                    Instant::now() - Duration::from_secs(1);
            }
            if case == 1 {
                c.inner
                    .lock()
                    .unwrap()
                    .alert_last
                    .as_mut()
                    .unwrap()
                    .observation
                    .window
                    .finished_unix_ms -= 6000;
            }
            if case == 2 {
                assert!(
                    c.act(
                        "owner",
                        l,
                        ActionRequest {
                            device: k.clone(),
                            expected_revision: Some(b.observation.revision),
                            action: Action::Home
                        }
                    )
                    .is_err()
                );
            }
            let other = DeviceKey::new("other").unwrap();
            assert!(
                c.tap_alert(
                    "owner",
                    l,
                    if case == 3 { &other } else { &k },
                    b.observation.revision,
                    request()
                )
                .is_err()
            );
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
        }
    }
    #[test]
    fn released_lease_never_reuses_old_alert_authority() {
        let (c, k, l) = setup(0);
        let b = c.observe_alert(&k).unwrap();
        c.release("owner", l).unwrap();
        let next = c.acquire("owner".into(), Duration::from_secs(30)).unwrap();
        assert!(
            c.tap_alert("owner", next, &k, b.observation.revision, request())
                .is_err()
        );
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
    }
}
