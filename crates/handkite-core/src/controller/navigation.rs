use super::*;
use serde::{Deserialize, Serialize};

/// A revision for app navigation only. It cannot admit element or coordinate input.
#[derive(Clone, Serialize, Deserialize)]
pub struct NavigationObservation {
    pub device: DeviceKey,
    pub revision: Revision,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub screenshot: ScreenImage,
}

pub struct NavigationResult {
    pub receipt: TransportReceipt,
    pub effect: ObservedEffect,
    pub after: Option<NavigationObservation>,
}

fn ready(state: &DeviceState) -> bool {
    state.lock == LockState::Unlocked
        && state.wda_ready
        && state
            .foreground_app
            .as_deref()
            .is_some_and(|app| !app.is_empty())
}

impl<A: DeviceAdapter> Controller<A> {
    fn accept_navigation(
        &self,
        inner: &mut Inner<A>,
        device: &DeviceKey,
        raw: RawObservation,
    ) -> Result<NavigationObservation, ControllerError> {
        if !raw.accessibility_tree.is_empty() || raw.screenshot.bytes.is_empty() {
            return Err(ControllerError::Adapter);
        }
        let observation = self.accept_capture(inner, device, raw)?;
        Ok(NavigationObservation {
            device: observation.device,
            revision: observation.revision,
            window: observation.window,
            state: observation.state,
            screenshot: observation.screenshot,
        })
    }

    pub fn observe_navigation(
        &self,
        device: &DeviceKey,
    ) -> Result<NavigationObservation, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        let raw = inner
            .adapter
            .observe_navigation()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let observation = self.accept_navigation(&mut inner, device, raw)?;
        inner.navigation_last = Some(observation.clone());
        Ok(observation)
    }

    pub fn navigate(
        &self,
        owner: &str,
        lease_id: LeaseId,
        request: ActionRequest,
    ) -> Result<NavigationResult, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&request.action, Action::Home | Action::LaunchApp { .. }) {
            return Err(ControllerError::LockedAction);
        }
        if !matches!(&inner.lease, Some(l) if l.id == lease_id && l.owner == owner && l.expires_at > Instant::now())
        {
            return Err(ControllerError::InvalidLease);
        }
        if request.device != self.binding.key {
            return Err(ControllerError::DeviceMismatch);
        }
        let revision = request
            .expected_revision
            .ok_or(ControllerError::StaleRevision)?;
        if revision != inner.revision {
            return Err(ControllerError::StaleRevision);
        }
        let (window, state) = if let Some(last) = inner
            .navigation_last
            .as_ref()
            .filter(|last| last.revision == revision)
        {
            (last.window.clone(), last.state.clone())
        } else if let Some(last) = inner
            .last
            .as_ref()
            .or(inner.target_last.as_ref())
            .filter(|last| last.revision == revision)
        {
            (last.window.clone(), last.state.clone())
        } else {
            return Err(ControllerError::StaleRevision);
        };
        self.validate_window(&window)?;
        if !ready(&state) {
            return Err(ControllerError::LockedAction);
        }
        let (raw, admission) = inner
            .adapter
            .prepare_navigation(&request.action)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let fresh = self.accept_navigation(&mut inner, &request.device, raw)?;
        if fresh.state != state {
            return Err(ControllerError::StaleObservation);
        }
        let until = match (&request.action, admission) {
            (Action::Home, ActionAdmission::NativeHome { until })
            | (Action::LaunchApp { .. }, ActionAdmission::NativeAppLaunch { until }) => until,
            _ => return Err(ControllerError::LockedAction),
        };
        let deadline = navigation_deadline(
            &fresh.window,
            self.max_age_ms,
            until,
            inner.lease.as_ref().unwrap().expires_at,
        )?;
        if deadline <= Instant::now() {
            return Err(ControllerError::StaleObservation);
        }
        // Consume every input baseline before dispatch, including on a lost reply.
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        let receipt = inner
            .adapter
            .execute_navigation(
                &request.action,
                deadline,
                admission,
                fresh.state.foreground_app.as_deref().unwrap(),
            )
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let after = match inner.adapter.observe_navigation() {
            Ok(raw) => match self.accept_navigation(&mut inner, &request.device, raw) {
                Ok(after) => Some(after),
                Err(ControllerError::DeviceMismatch) => {
                    return Err(ControllerError::DeviceMismatch);
                }
                Err(_) => None,
            },
            Err(_) => None,
        };
        let destination = match &request.action {
            Action::Home => "com.apple.springboard",
            Action::LaunchApp { bundle_id } => bundle_id,
            _ => unreachable!(),
        };
        let effect = if receipt.accepted == Some(true)
            && after.as_ref().is_some_and(|after| {
                ready(&after.state) && after.state.foreground_app.as_deref() == Some(destination)
            }) {
            ObservedEffect::Changed
        } else {
            ObservedEffect::Unconfirmed
        };
        inner.navigation_last = after.clone();
        let renewal = ActionResult {
            receipt: receipt.clone(),
            after: None,
            after_image: None,
            native_lock: None,
            effect,
        };
        renew_verified_action(&mut inner.lease, owner, lease_id, &renewal, Instant::now());
        Ok(NavigationResult {
            receipt,
            effect,
            after,
        })
    }
}

fn navigation_deadline(
    window: &ObservationWindow,
    max_age_ms: u64,
    admission: Instant,
    lease: Instant,
) -> Result<Instant, ControllerError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ControllerError::StaleObservation)?
        .as_millis() as u64;
    let remaining = now
        .checked_sub(window.finished_unix_ms)
        .and_then(|age| max_age_ms.checked_sub(age))
        .filter(|remaining| *remaining > 0)
        .ok_or(ControllerError::StaleObservation)?;
    Ok((Instant::now() + Duration::from_millis(remaining))
        .min(admission)
        .min(lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ImageFormat;
    #[test]
    fn near_expiry_navigation_cannot_outlive_capture_budget() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let window = ObservationWindow {
            started_unix_ms: now - 990,
            finished_unix_ms: now - 990,
        };
        let far = Instant::now() + Duration::from_secs(10);
        let deadline = navigation_deadline(&window, 1000, far, far).unwrap();
        assert!(deadline <= Instant::now() + Duration::from_millis(10));
        assert!(navigation_deadline(&window, 900, far, far).is_err());
    }
    struct Adapter {
        app: String,
        locked: bool,
        sends: usize,
        lost: bool,
        lock_after: bool,
        pixels: u8,
    }
    impl DeviceAdapter for Adapter {
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            let id = HardwareId::new("fixture").unwrap();
            Ok((id.clone(), id))
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            panic!("navigation must not fetch a tree")
        }
        fn observe_navigation(&mut self) -> Result<RawObservation, AdapterError> {
            self.pixels = self.pixels.wrapping_add(1);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let (connected_usb, wda_target) = self.identities()?;
            Ok(RawObservation {
                connected_usb,
                wda_target,
                window: ObservationWindow {
                    started_unix_ms: now,
                    finished_unix_ms: now,
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
                accessibility_tree: String::new(),
                screenshot: ScreenImage {
                    format: ImageFormat::Png,
                    bytes: vec![self.pixels],
                },
            })
        }
        fn prepare_navigation(
            &mut self,
            action: &Action,
        ) -> Result<(RawObservation, ActionAdmission), AdapterError> {
            let until = Instant::now() + Duration::from_secs(1);
            let admission = match action {
                Action::Home => ActionAdmission::NativeHome { until },
                Action::LaunchApp { .. } => ActionAdmission::NativeAppLaunch { until },
                _ => panic!(),
            };
            Ok((self.observe_navigation()?, admission))
        }
        fn execute_navigation(
            &mut self,
            action: &Action,
            deadline: Instant,
            _: ActionAdmission,
            app: &str,
        ) -> Result<TransportReceipt, AdapterError> {
            assert!(Instant::now() < deadline);
            assert_eq!(app, self.app);
            self.sends += 1;
            self.locked = self.lock_after;
            self.app = match action {
                Action::Home => "com.apple.springboard".into(),
                Action::LaunchApp { bundle_id } => bundle_id.clone(),
                _ => panic!(),
            };
            if self.lost {
                return Err(AdapterError);
            }
            Ok(TransportReceipt {
                accepted: Some(true),
                action_kind: action.kind().into(),
            })
        }
        fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
            panic!("wrong route")
        }
    }
    fn fixture() -> (Controller<Adapter>, DeviceKey, LeaseId) {
        let key = DeviceKey::new("phone").unwrap();
        let c = Controller::new(
            DeviceBinding {
                key: key.clone(),
                usb_id: HardwareId::new("fixture").unwrap(),
            },
            Adapter {
                app: "example.animated".into(),
                locked: false,
                sends: 0,
                lost: false,
                lock_after: false,
                pixels: 0,
            },
            5000,
            5000,
        );
        let lease = c
            .acquire_with_renewal("owner".into(), Duration::from_secs(30), true)
            .unwrap();
        (c, key, lease)
    }
    #[test]
    fn animated_navigation_needs_no_tree_and_returns_only_navigation_revision() {
        let (c, key, lease) = fixture();
        let before = c.observe_navigation(&key).unwrap();
        let result = c
            .navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key.clone(),
                    expected_revision: Some(before.revision),
                    action: Action::Home,
                },
            )
            .unwrap();
        assert_eq!(result.effect, ObservedEffect::Changed);
        let next = result.after.unwrap();
        assert_eq!(
            next.state.foreground_app.as_deref(),
            Some("com.apple.springboard")
        );
        assert!(matches!(
            c.act(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(next.revision),
                    action: Action::Tap {
                        at: Point { x: 1., y: 1. }
                    }
                }
            ),
            Err(ControllerError::StaleRevision)
        ));
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
    #[test]
    fn navigation_rejects_other_actions_stale_foreground_lock_and_lease() {
        for mode in 0..5 {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            let mut action = Action::Home;
            let mut revision = before.revision;
            match mode {
                0 => {
                    action = Action::TextCurrent {
                        value: "fixture".into(),
                    }
                }
                1 => revision = Revision(99),
                2 => c.inner.lock().unwrap().adapter.app = "other.app".into(),
                3 => c.inner.lock().unwrap().adapter.locked = true,
                _ => c.release("owner", lease).unwrap(),
            }
            assert!(
                c.navigate(
                    "owner",
                    lease,
                    ActionRequest {
                        device: key,
                        expected_revision: Some(revision),
                        action
                    }
                )
                .is_err()
            );
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
        }
    }
    #[test]
    fn locked_destination_is_visible_but_never_confirms_or_renews() {
        let (c, key, lease) = fixture();
        let before = c.observe_navigation(&key).unwrap();
        let expiry = c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at;
        c.inner.lock().unwrap().adapter.lock_after = true;
        let result = c
            .navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::Home,
                },
            )
            .unwrap();
        assert_eq!(result.effect, ObservedEffect::Unconfirmed);
        assert_eq!(result.after.unwrap().state.lock, LockState::Locked);
        assert_eq!(
            c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at,
            expiry
        );
    }

    #[test]
    fn uncertain_navigation_consumes_revision_without_replay() {
        let (c, key, lease) = fixture();
        let before = c.observe_navigation(&key).unwrap();
        c.inner.lock().unwrap().adapter.lost = true;
        for _ in 0..2 {
            assert!(
                c.navigate(
                    "owner",
                    lease,
                    ActionRequest {
                        device: key.clone(),
                        expected_revision: Some(before.revision),
                        action: Action::Home
                    }
                )
                .is_err()
            );
        }
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
}
