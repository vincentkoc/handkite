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

/// Internal capture outcome, never serialized as observation or input authority.
pub enum LaunchReadback {
    Observation(RawObservation),
    /// The discarded frame spans only the expected apps, with both native
    /// proofs Unlocked, all WDA lock reports false, destination proven last,
    /// and final ownership/session/runner identity unchanged.
    Transition,
}

struct Readback {
    observation: Option<NavigationObservation>,
    transition: bool,
}

fn ready(state: &DeviceState) -> bool {
    state.lock == LockState::Unlocked
        && state.wda_ready
        && state
            .foreground_app
            .as_deref()
            .is_some_and(|app| !app.is_empty())
}

// This limits starting a second read, not the existing per-read timeouts.
const NAVIGATION_RETRY_START_WINDOW: Duration = Duration::from_secs(2);

fn retry_navigation_read(
    action: &Action,
    accepted: Option<bool>,
    after: Option<&NavigationObservation>,
    source: &str,
    elapsed: Duration,
    transition: bool,
) -> bool {
    if accepted != Some(true) || elapsed >= NAVIGATION_RETRY_START_WINDOW {
        return false;
    }
    let destination = match action {
        Action::Home => "com.apple.springboard",
        Action::LaunchApp { bundle_id } => bundle_id,
        _ => return false,
    };
    let Some(after) = after else {
        // An ordinary missing capture may mean ownership or session failure.
        return matches!(action, Action::Home) || (transition && source != destination);
    };
    ready(&after.state)
        && source != destination
        && after.state.foreground_app.as_deref() == Some(source)
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

    fn navigation_readback(
        &self,
        inner: &mut Inner<A>,
        device: &DeviceKey,
        home_retry: Option<bool>,
        launch: Option<(&str, &str)>,
        launch_retry: bool,
    ) -> Result<Readback, ControllerError> {
        let raw = match (home_retry, launch) {
            (Some(retry), _) => inner
                .adapter
                .observe_home_readback(retry)
                .map(LaunchReadback::Observation),
            (None, Some((source, destination))) if launch_retry => inner
                .adapter
                .observe_launch_retry(source, destination)
                .map(LaunchReadback::Observation),
            (None, Some((source, destination))) => {
                inner.adapter.observe_launch_readback(source, destination)
            }
            (None, None) => inner
                .adapter
                .observe_navigation()
                .map(LaunchReadback::Observation),
        };
        match raw {
            Ok(LaunchReadback::Transition) if launch.is_some() && !launch_retry => Ok(Readback {
                observation: None,
                transition: true,
            }),
            Ok(LaunchReadback::Observation(raw)) => {
                let observation = match self.accept_navigation(inner, device, raw) {
                    Ok(after) => Some(after),
                    Err(ControllerError::DeviceMismatch) => {
                        return Err(ControllerError::DeviceMismatch);
                    }
                    Err(_) => None,
                };
                Ok(Readback {
                    observation,
                    transition: false,
                })
            }
            _ => Ok(Readback {
                observation: None,
                transition: false,
            }),
        }
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
        let verification_started = Instant::now();
        let home_readback =
            matches!(request.action, Action::Home) && receipt.accepted == Some(true);
        let source = fresh.state.foreground_app.as_deref().unwrap();
        let launch = match &request.action {
            Action::LaunchApp { bundle_id } if receipt.accepted == Some(true) => {
                Some((source, bundle_id.as_str()))
            }
            _ => None,
        };
        let first = self.navigation_readback(
            &mut inner,
            &request.device,
            home_readback.then_some(false),
            launch,
            false,
        )?;
        let mut after = first.observation;
        if retry_navigation_read(
            &request.action,
            receipt.accepted,
            after.as_ref(),
            fresh.state.foreground_app.as_deref().unwrap(),
            verification_started.elapsed(),
            first.transition,
        ) {
            // Only read again. The input baseline stays consumed, and only this
            // final read can supply a revision or confirm the navigation effect.
            after = self
                .navigation_readback(
                    &mut inner,
                    &request.device,
                    home_readback.then_some(true),
                    launch,
                    true,
                )?
                .observation;
        }
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
    #[derive(Clone, Copy)]
    enum PostCapture {
        Stable,
        FailOnce,
        OldOnce,
        OldAlways,
        OldThenFail,
        TransitionOnce,
        TransitionThenFail,
        TransitionThenOld,
        TransitionThenLocked,
        TransitionThenWrongDevice,
        FailAlways,
        Other,
        WrongDevice,
    }
    struct Adapter {
        app: String,
        locked: bool,
        sends: usize,
        lost: bool,
        lock_after: bool,
        pixels: u8,
        post_reads: usize,
        post_capture: PostCapture,
        accepted: Option<bool>,
        home_reads: Vec<bool>,
        launch_reads: usize,
        launch_retries: Vec<(String, String)>,
    }
    impl DeviceAdapter for Adapter {
        fn observe_home_readback(&mut self, retry: bool) -> Result<RawObservation, AdapterError> {
            self.home_reads.push(retry);
            self.observe_navigation()
        }
        fn observe_launch_readback(
            &mut self,
            source: &str,
            destination: &str,
        ) -> Result<LaunchReadback, AdapterError> {
            self.launch_reads += 1;
            assert_eq!(source, "example.animated");
            assert_eq!(destination, self.app);
            if matches!(
                self.post_capture,
                PostCapture::TransitionOnce
                    | PostCapture::TransitionThenFail
                    | PostCapture::TransitionThenOld
                    | PostCapture::TransitionThenLocked
                    | PostCapture::TransitionThenWrongDevice
            ) {
                self.post_reads += 1;
                return Ok(LaunchReadback::Transition);
            }
            self.observe_navigation().map(LaunchReadback::Observation)
        }
        fn observe_launch_retry(
            &mut self,
            source: &str,
            destination: &str,
        ) -> Result<RawObservation, AdapterError> {
            self.launch_retries
                .push((source.into(), destination.into()));
            self.observe_navigation()
        }
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            let id = HardwareId::new("fixture").unwrap();
            Ok((id.clone(), id))
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            panic!("navigation must not fetch a tree")
        }
        fn observe_navigation(&mut self) -> Result<RawObservation, AdapterError> {
            if self.sends > 0 {
                self.post_reads += 1;
                if matches!(self.post_capture, PostCapture::FailAlways)
                    || (matches!(self.post_capture, PostCapture::FailOnce) && self.post_reads == 1)
                    || (matches!(self.post_capture, PostCapture::OldThenFail)
                        && self.post_reads > 1)
                    || (matches!(self.post_capture, PostCapture::TransitionThenFail)
                        && self.post_reads > 1)
                {
                    return Err(AdapterError);
                }
                if matches!(self.post_capture, PostCapture::TransitionThenLocked)
                    && self.post_reads > 1
                {
                    self.locked = true;
                }
            }
            self.pixels = self.pixels.wrapping_add(1);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let (connected_usb, mut wda_target) = self.identities()?;
            if self.sends > 0
                && matches!(
                    self.post_capture,
                    PostCapture::WrongDevice | PostCapture::TransitionThenWrongDevice
                )
            {
                wda_target = HardwareId::new("other-device").unwrap();
            }
            let app = if self.sends > 0 && matches!(self.post_capture, PostCapture::Other) {
                "other.app".into()
            } else if self.sends > 0
                && (matches!(
                    self.post_capture,
                    PostCapture::OldAlways | PostCapture::TransitionThenOld
                ) || (self.post_reads == 1
                    && matches!(
                        self.post_capture,
                        PostCapture::OldOnce | PostCapture::OldThenFail
                    )))
            {
                "example.animated".into()
            } else {
                self.app.clone()
            };
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
                    foreground_app: Some(app),
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
                accepted: self.accepted,
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
                post_reads: 0,
                post_capture: PostCapture::Stable,
                accepted: Some(true),
                home_reads: Vec::new(),
                launch_reads: 0,
                launch_retries: Vec::new(),
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
    fn home_proof_handoff_is_only_the_existing_acknowledged_second_read() {
        for (launch, accepted, mode, expected) in [
            (false, Some(true), PostCapture::FailOnce, vec![false, true]),
            (false, Some(true), PostCapture::Stable, vec![false]),
            (false, Some(false), PostCapture::FailOnce, vec![]),
            (true, Some(true), PostCapture::Stable, vec![]),
        ] {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            {
                let mut inner = c.inner.lock().unwrap();
                inner.adapter.accepted = accepted;
                inner.adapter.post_capture = mode;
            }
            c.navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: if launch {
                        Action::LaunchApp {
                            bundle_id: "example.target".into(),
                        }
                    } else {
                        Action::Home
                    },
                },
            )
            .unwrap();
            let inner = c.inner.lock().unwrap();
            assert_eq!(inner.adapter.home_reads, expected);
            assert_eq!(inner.adapter.sends, 1);
            assert!(inner.adapter.post_reads <= 2);
            assert!(inner.adapter.launch_retries.is_empty());
        }
    }

    #[test]
    fn home_verification_recovers_one_transition_read_without_resending_input() {
        for mode in [PostCapture::FailOnce, PostCapture::OldOnce] {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            c.inner.lock().unwrap().adapter.post_capture = mode;
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
            assert_eq!(result.effect, ObservedEffect::Changed);
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
            assert_eq!(c.inner.lock().unwrap().adapter.post_reads, 2);
        }
    }

    #[test]
    fn launch_verification_recovers_one_old_app_frame_without_resending() {
        let (c, key, lease) = fixture();
        let before = c.observe_navigation(&key).unwrap();
        c.inner.lock().unwrap().adapter.post_capture = PostCapture::OldOnce;
        let result = c
            .navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::LaunchApp {
                        bundle_id: "example.target".into(),
                    },
                },
            )
            .unwrap();
        assert_eq!(result.effect, ObservedEffect::Changed);
        assert_eq!(
            result.after.unwrap().state.foreground_app.as_deref(),
            Some("example.target")
        );
        let inner = c.inner.lock().unwrap();
        assert_eq!(inner.adapter.sends, 1);
        assert_eq!(inner.adapter.post_reads, 2);
        assert!(inner.adapter.home_reads.is_empty());
        assert_eq!(
            inner.adapter.launch_retries,
            [("example.animated".into(), "example.target".into())]
        );
    }

    #[test]
    fn launch_retry_keeps_negative_evidence_and_never_uses_home_proofs() {
        for (mode, accepted, locked, reads) in [
            (PostCapture::OldAlways, Some(true), false, 2),
            (PostCapture::OldThenFail, Some(true), false, 2),
            (PostCapture::Other, Some(true), false, 1),
            (PostCapture::WrongDevice, Some(true), false, 1),
            (PostCapture::OldOnce, Some(false), false, 1),
            (PostCapture::OldOnce, None, false, 1),
            (PostCapture::OldOnce, Some(true), true, 1),
            (PostCapture::FailOnce, Some(true), false, 1),
        ] {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            {
                let mut inner = c.inner.lock().unwrap();
                inner.adapter.post_capture = mode;
                inner.adapter.accepted = accepted;
                inner.adapter.lock_after = locked;
            }
            let result = c.navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::LaunchApp {
                        bundle_id: "example.target".into(),
                    },
                },
            );
            if matches!(mode, PostCapture::WrongDevice) {
                assert!(matches!(result, Err(ControllerError::DeviceMismatch)));
            } else {
                assert_eq!(result.unwrap().effect, ObservedEffect::Unconfirmed);
            }
            let inner = c.inner.lock().unwrap();
            assert_eq!(inner.adapter.sends, 1);
            assert_eq!(inner.adapter.post_reads, reads);
            assert_eq!(
                inner.adapter.launch_reads,
                usize::from(accepted == Some(true))
            );
            assert!(inner.adapter.home_reads.is_empty());
            assert_eq!(inner.adapter.launch_retries.len(), reads.saturating_sub(1));
        }
    }

    #[test]
    fn launch_retry_requires_a_ready_source_frame_inside_the_start_window() {
        let (c, key, _) = fixture();
        let old = c.observe_navigation(&key).unwrap();
        let launch = Action::LaunchApp {
            bundle_id: "example.target".into(),
        };
        let source = "example.animated";
        let retry = |after: Option<&NavigationObservation>, elapsed| {
            retry_navigation_read(&launch, Some(true), after, source, elapsed, false)
        };
        assert!(retry(
            Some(&old),
            NAVIGATION_RETRY_START_WINDOW - Duration::from_nanos(1)
        ));
        assert!(!retry(Some(&old), NAVIGATION_RETRY_START_WINDOW));
        assert!(!retry(None, Duration::ZERO));
        for lock in [LockState::Locked, LockState::Uncertain] {
            let mut after = old.clone();
            after.state.lock = lock;
            assert!(!retry(Some(&after), Duration::ZERO));
        }
        let mut after = old.clone();
        after.state.wda_ready = false;
        assert!(!retry(Some(&after), Duration::ZERO));
        for app in [
            None,
            Some("example.target".into()),
            Some("other.app".into()),
        ] {
            let mut after = old.clone();
            after.state.foreground_app = app;
            assert!(!retry(Some(&after), Duration::ZERO));
        }
        assert!(!retry_navigation_read(
            &Action::LaunchApp {
                bundle_id: source.into()
            },
            Some(true),
            Some(&old),
            source,
            Duration::ZERO,
            false,
        ));
    }

    #[test]
    fn launch_transition_is_read_only_single_use_and_has_no_revision() {
        for mode in [
            PostCapture::TransitionOnce,
            PostCapture::TransitionThenFail,
            PostCapture::TransitionThenOld,
            PostCapture::TransitionThenLocked,
            PostCapture::TransitionThenWrongDevice,
        ] {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            c.inner.lock().unwrap().adapter.post_capture = mode;
            let result = c.navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::LaunchApp {
                        bundle_id: "example.target".into(),
                    },
                },
            );
            match mode {
                PostCapture::TransitionOnce => {
                    let result = result.unwrap();
                    assert_eq!(result.effect, ObservedEffect::Changed);
                    assert_eq!(result.after.unwrap().revision.0, before.revision.0 + 2);
                }
                PostCapture::TransitionThenWrongDevice => {
                    assert!(matches!(result, Err(ControllerError::DeviceMismatch)))
                }
                _ => assert_eq!(result.unwrap().effect, ObservedEffect::Unconfirmed),
            }
            let inner = c.inner.lock().unwrap();
            assert_eq!(inner.adapter.sends, 1);
            assert_eq!(inner.adapter.launch_reads, 1);
            assert_eq!(inner.adapter.post_reads, 2);
            assert!(inner.adapter.home_reads.is_empty());
        }
    }

    #[test]
    fn transition_marker_still_requires_ack_and_time_remaining() {
        let launch = Action::LaunchApp {
            bundle_id: "example.target".into(),
        };
        assert!(retry_navigation_read(
            &launch,
            Some(true),
            None,
            "example.animated",
            Duration::ZERO,
            true
        ));
        for ack in [None, Some(false)] {
            assert!(!retry_navigation_read(
                &launch,
                ack,
                None,
                "example.animated",
                Duration::ZERO,
                true
            ));
        }
        assert!(!retry_navigation_read(
            &launch,
            Some(true),
            None,
            "example.animated",
            NAVIGATION_RETRY_START_WINDOW,
            true
        ));
        assert!(!retry_navigation_read(
            &launch,
            Some(true),
            None,
            "example.target",
            Duration::ZERO,
            true
        ));
        assert!(!retry_navigation_read(
            &launch,
            Some(true),
            None,
            "example.animated",
            Duration::ZERO,
            false
        ));
    }

    #[test]
    fn home_read_retry_keeps_negative_evidence_and_stops_after_two_reads() {
        for (mode, accepted, launch, locked, reads) in [
            (PostCapture::FailAlways, Some(true), false, false, 2),
            (PostCapture::Other, Some(true), false, false, 1),
            (PostCapture::WrongDevice, Some(true), false, false, 1),
            (PostCapture::FailOnce, Some(false), false, false, 1),
            (PostCapture::FailOnce, Some(true), true, false, 1),
            (PostCapture::Stable, Some(true), false, true, 1),
        ] {
            let (c, key, lease) = fixture();
            let before = c.observe_navigation(&key).unwrap();
            {
                let mut inner = c.inner.lock().unwrap();
                inner.adapter.post_capture = mode;
                inner.adapter.accepted = accepted;
                inner.adapter.lock_after = locked;
            }
            let action = if launch {
                Action::LaunchApp {
                    bundle_id: "example.target".into(),
                }
            } else {
                Action::Home
            };
            let result = c.navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action,
                },
            );
            if matches!(mode, PostCapture::WrongDevice) {
                assert!(matches!(result, Err(ControllerError::DeviceMismatch)));
            } else {
                assert_eq!(result.unwrap().effect, ObservedEffect::Unconfirmed);
            }
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
            assert_eq!(c.inner.lock().unwrap().adapter.post_reads, reads);
        }
    }

    #[test]
    fn home_read_retry_cannot_start_at_or_after_its_window() {
        assert!(retry_navigation_read(
            &Action::Home,
            Some(true),
            None,
            "example.app",
            Duration::ZERO,
            false,
        ));
        for elapsed in [
            NAVIGATION_RETRY_START_WINDOW,
            NAVIGATION_RETRY_START_WINDOW + Duration::from_millis(1),
        ] {
            assert!(!retry_navigation_read(
                &Action::Home,
                Some(true),
                None,
                "example.app",
                elapsed,
                false,
            ));
        }
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
    fn uncertain_home_baseline_refuses_before_any_input_on_an_unlocked_device() {
        let (c, key, lease) = fixture();
        c.inner.lock().unwrap().adapter.app = "com.apple.springboard".into();
        let before = c.observe_navigation(&key).unwrap();
        c.inner
            .lock()
            .unwrap()
            .navigation_last
            .as_mut()
            .unwrap()
            .state
            .lock = LockState::Uncertain;
        assert!(!c.inner.lock().unwrap().adapter.locked);
        assert!(matches!(
            c.navigate(
                "owner",
                lease,
                ActionRequest {
                    device: key,
                    expected_revision: Some(before.revision),
                    action: Action::Home,
                }
            ),
            Err(ControllerError::LockedAction)
        ));
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
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
