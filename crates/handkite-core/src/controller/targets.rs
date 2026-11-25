use super::*;
use serde::{Deserialize, Serialize};

/// An opaque, one-use target with explicit XML or OCR provenance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementTarget {
    pub target_id: String,
    #[serde(default = "xml_source")]
    pub source: String,
    pub kind: String,
    pub label: String,
    pub can_type: bool,
    /// This active editor is admitted through semantic text checks, never tapping.
    #[serde(default)]
    pub text_only: bool,
    /// Logical x, y, width, height; metadata only, never an input fallback.
    pub rect: [f64; 4],
    pub value: Option<String>,
    pub value_truncated: bool,
}

fn xml_source() -> String {
    "xml".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementAction {
    Tap { target_id: String },
    Text { target_id: String, value: String },
}

impl ElementAction {
    pub fn target_id(&self) -> &str {
        match self {
            Self::Tap { target_id } | Self::Text { target_id, .. } => target_id,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Tap { .. } => "tap_element",
            Self::Text { .. } => "text_element",
        }
    }
    pub fn valid(&self) -> bool {
        let id = self.target_id();
        id.len() == 34
            && id.starts_with("t_")
            && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
            && match self {
                Self::Text { value, .. } => crate::protocol::injectable_text(value),
                _ => true,
            }
    }
}

pub struct TargetObservation {
    pub observation: Observation,
    pub targets: Vec<ElementTarget>,
}

pub struct TargetActionResult {
    pub result: ActionResult,
    pub next: Option<TargetObservation>,
}

pub struct TargetCompletion {
    pub readback: Result<TextReadback, AdapterError>,
    pub next: Result<(RawObservation, Vec<ElementTarget>), AdapterError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetEffect {
    pub token: u64,
    pub target_id: String,
    pub effect: ObservedEffect,
    pub region: [f64; 4],
}

#[derive(Clone)]
pub struct TargetEffectReport {
    pub update: TargetEffect,
    pub proof: Option<std::sync::Arc<RawImageObservation>>,
    pub completed_unix_ms: u64,
}

pub struct PreparedTarget {
    pub evidence: PreparedText,
}

impl<A: DeviceAdapter> Controller<A> {
    pub fn observe_targets(
        &self,
        device: &DeviceKey,
    ) -> Result<TargetObservation, ControllerError> {
        self.observe_targets_since(device, None)
    }

    pub fn observe_targets_since(
        &self,
        device: &DeviceKey,
        since: Option<Revision>,
    ) -> Result<TargetObservation, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        inner.alert_last = None;
        let (raw, targets) = inner
            .adapter
            .observe_targets_since(since)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let observation = self.accept_capture(&mut inner, device, raw)?;
        if !public_state(&observation.state) {
            return Err(ControllerError::LockedAction);
        }
        inner
            .adapter
            .bind_targets(observation.revision)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        inner.target_last = Some(observation.clone());
        Ok(TargetObservation {
            observation,
            targets,
        })
    }

    pub fn act_element(
        &self,
        owner: &str,
        lease_id: LeaseId,
        device: &DeviceKey,
        revision: Revision,
        action: ElementAction,
    ) -> Result<TargetActionResult, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&inner.lease, Some(l) if l.id == lease_id && l.owner == owner && l.expires_at > Instant::now())
        {
            return Err(ControllerError::InvalidLease);
        }
        if device != &self.binding.key {
            return Err(ControllerError::DeviceMismatch);
        }
        inner
            .adapter
            .settle_target_effects_for(action.target_id())
            .map_err(|_| Self::adapter_error(&mut inner))?;
        self.refresh_target_effects(&mut inner);
        if !action.valid() {
            return Err(ControllerError::Adapter);
        }
        let baseline = inner
            .target_last
            .clone()
            .filter(|last| last.revision == revision)
            .ok_or(ControllerError::StaleRevision)?;
        self.validate_window(&baseline.window)?;
        if !public_state(&baseline.state) {
            return Err(ControllerError::LockedAction);
        }
        let prepared = inner
            .adapter
            .prepare_target(revision, &action, &baseline)
            .map_err(|_| Self::adapter_error(&mut inner))?
            .evidence;
        self.binding
            .verify(device, &prepared.connected_usb, &prepared.wda_target)
            .map_err(|_| ControllerError::DeviceMismatch)?;
        self.validate_window(&prepared.window)?;
        if prepared.state != baseline.state || !public_state(&prepared.state) {
            return Err(ControllerError::StaleObservation);
        }
        let lease = inner
            .lease
            .as_ref()
            .filter(|l| l.expires_at > Instant::now())
            .ok_or(ControllerError::InvalidLease)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::StaleObservation)?
            .as_millis() as u64;
        let remaining = self
            .max_age_ms
            .checked_sub(
                now.checked_sub(prepared.window.finished_unix_ms)
                    .ok_or(ControllerError::StaleObservation)?,
            )
            .filter(|n| *n > 0)
            .ok_or(ControllerError::StaleObservation)?;
        let deadline = (Instant::now() + Duration::from_millis(remaining))
            .min(lease.expires_at)
            .min(prepared.valid_until);
        if deadline <= Instant::now() {
            return Err(ControllerError::StaleObservation);
        }
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
        inner.alert_last = None;
        let receipt = inner
            .adapter
            .execute_target(&action, prepared.token, deadline)
            .map_err(|_| Self::adapter_error(&mut inner))?;
        let completed = inner.adapter.complete_target(prepared.token);
        let deferred = receipt.accepted == Some(true)
            && inner.adapter.target_effect_updates().iter().any(|report| {
                report.update.target_id == action.target_id()
                    && report.update.token == prepared.token
            });
        let deferred_effect = if deferred {
            inner.pending_target_lease = Some((
                owner.to_owned(),
                lease_id,
                action.target_id().to_owned(),
                prepared.token,
            ));
            self.refresh_target_effects(&mut inner)
                .into_iter()
                .find(|update| {
                    update.target_id == action.target_id() && update.token == prepared.token
                })
                .map(|update| update.effect)
        } else {
            None
        };
        let readback = completed.readback;
        let (after_image, verified) = match readback {
            Ok(readback) => match self.accept_image(device, readback.image) {
                Ok(image) => (Some(image), readback.input_verified),
                // Dispatch has already acknowledged. Discard invalid proof,
                // without losing the receipt or independently valid next targets.
                Err(_) => (None, false),
            },
            Err(_) => (None, false),
        };
        let next = completed
            .next
            .ok()
            .filter(|_| !deferred)
            .and_then(|(raw, targets)| {
                let observation = self.accept_capture(&mut inner, device, raw).ok()?;
                if !public_state(&observation.state) {
                    return None;
                }
                inner.adapter.bind_targets(observation.revision).ok()?;
                inner.target_last = Some(observation.clone());
                Some(TargetObservation {
                    observation,
                    targets,
                })
            });
        let result = ActionResult {
            effect: if let Some(effect) = deferred_effect {
                effect
            } else if verified && receipt.accepted == Some(true) {
                ObservedEffect::Changed
            } else {
                ObservedEffect::Unconfirmed
            },
            receipt,
            after: None,
            after_image,
            native_lock: None,
        };
        if !deferred {
            renew_verified_action(&mut inner.lease, owner, lease_id, &result, Instant::now());
        }
        Ok(TargetActionResult { result, next })
    }

    pub fn target_effect_updates(&self) -> Vec<TargetEffect> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        self.refresh_target_effects(&mut inner)
    }

    fn refresh_target_effects(&self, inner: &mut Inner<A>) -> Vec<TargetEffect> {
        let reports = inner.adapter.target_effect_updates();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let updates: Vec<_> = reports
            .into_iter()
            .map(|report| {
                let mut update = report.update;
                if update.effect == ObservedEffect::Changed {
                    let valid = report.proof.as_ref().is_some_and(|proof| {
                        self.binding
                            .verify(&self.binding.key, &proof.connected_usb, &proof.wda_target)
                            .is_ok()
                            && !proof.screenshot.bytes.is_empty()
                            && proof
                                .window
                                .duration_ms()
                                .is_ok_and(|ms| ms <= self.max_window_ms)
                            && report.completed_unix_ms <= now
                            && report
                                .completed_unix_ms
                                .checked_sub(proof.window.finished_unix_ms)
                                .is_some_and(|age| age <= self.max_age_ms)
                    });
                    if !valid {
                        update.effect = ObservedEffect::Unconfirmed;
                    }
                }
                update
            })
            .collect();
        if let Some((owner, id, target, token)) = &inner.pending_target_lease
            && let Some(update) = updates.iter().find(|u| {
                &u.target_id == target && u.token == *token && u.effect != ObservedEffect::Pending
            })
        {
            if update.effect == ObservedEffect::Changed
                && let Some(lease) = &mut inner.lease
                && lease.id == *id
                && lease.owner == *owner
                && lease.expires_at > Instant::now()
                && let Some(ttl) = lease.renew_on_success
            {
                lease.expires_at = Instant::now() + ttl;
            }
            inner.pending_target_lease = None;
        }
        updates
    }
}

fn public_state(state: &DeviceState) -> bool {
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
    const ID: &str = "t_0123456789abcdef0123456789abcdef";
    #[derive(Default)]
    struct Adapter {
        sends: usize,
        fail_send: bool,
        invalid: u8,
        bound: Option<Revision>,
        prepared: bool,
        captures: usize,
        combined: bool,
        verified: bool,
        stale_readback: bool,
        updates: Vec<TargetEffectReport>,
        fresh_report_on_read: bool,
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
                foreground_app: Some("example.app".into()),
            },
            accessibility_tree: "<public/>".into(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![1],
            },
        }
    }
    impl DeviceAdapter for Adapter {
        fn target_effect_updates(&mut self) -> Vec<TargetEffectReport> {
            if self.fresh_report_on_read {
                std::thread::sleep(Duration::from_millis(2));
                self.updates = vec![effect_report(ObservedEffect::Changed)];
            }
            self.updates.clone()
        }
        fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
            Ok((raw().connected_usb, raw().wda_target))
        }
        fn observe(&mut self) -> Result<RawObservation, AdapterError> {
            Ok(raw())
        }
        fn observe_targets(
            &mut self,
        ) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
            self.captures += 1;
            if self.captures > 1 {
                return Err(AdapterError);
            }
            Ok((
                raw(),
                vec![ElementTarget {
                    source: "xml".into(),
                    target_id: ID.into(),
                    kind: "Button".into(),
                    label: "Choose".into(),
                    can_type: false,
                    text_only: false,
                    rect: [0.0, 0.0, 1.0, 1.0],
                    value: None,
                    value_truncated: false,
                }],
            ))
        }
        fn bind_targets(&mut self, revision: Revision) -> Result<(), AdapterError> {
            self.bound = Some(revision);
            Ok(())
        }
        fn prepare_target(
            &mut self,
            revision: Revision,
            _: &ElementAction,
            _: &Observation,
        ) -> Result<PreparedTarget, AdapterError> {
            if self.bound != Some(revision) || self.invalid == 1 {
                return Err(AdapterError);
            }
            self.prepared = true;
            let mut raw = raw();
            if self.invalid == 2 {
                raw.state.lock = LockState::Locked;
            }
            if self.invalid == 3 {
                raw.state.foreground_app = Some("com.apple.springboard".into());
            }
            if self.invalid == 4 {
                raw.wda_target = HardwareId::new("other").unwrap();
            }
            Ok(PreparedTarget {
                evidence: PreparedText {
                    connected_usb: raw.connected_usb,
                    wda_target: raw.wda_target,
                    window: raw.window,
                    state: raw.state,
                    token: 1,
                    valid_until: if self.invalid == 5 {
                        Instant::now() - Duration::from_secs(1)
                    } else {
                        Instant::now() + Duration::from_secs(1)
                    },
                },
            })
        }
        fn execute_target(
            &mut self,
            _: &ElementAction,
            _: u64,
            _: Instant,
        ) -> Result<TransportReceipt, AdapterError> {
            assert!(self.prepared);
            self.prepared = false;
            self.bound = None;
            self.sends += 1;
            if self.fail_send {
                Err(AdapterError)
            } else {
                Ok(TransportReceipt {
                    accepted: Some(true),
                    action_kind: "tap_element".into(),
                })
            }
        }
        fn complete_target(&mut self, token: u64) -> TargetCompletion {
            if !self.combined {
                let readback = self.target_readback(token);
                let next = self.observe_targets();
                return TargetCompletion { readback, next };
            }
            self.captures += 1;
            let raw = raw();
            let mut image = RawImageObservation {
                connected_usb: raw.connected_usb.clone(),
                wda_target: raw.wda_target.clone(),
                window: raw.window.clone(),
                screenshot: raw.screenshot.clone(),
            };
            if self.stale_readback {
                image.window.started_unix_ms -= 10_000;
                image.window.finished_unix_ms -= 10_000;
            }
            TargetCompletion {
                readback: Ok(TextReadback {
                    image,
                    input_verified: self.verified,
                }),
                next: Ok((raw, vec![])),
            }
        }
        fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
            panic!("no legacy fallback")
        }
    }
    fn fixture(adapter: Adapter) -> (Controller<Adapter>, DeviceKey, LeaseId, Revision) {
        let device = DeviceKey::new("phone").unwrap();
        let c = Controller::new(
            DeviceBinding {
                key: device.clone(),
                usb_id: raw().connected_usb,
            },
            adapter,
            1000,
            5000,
        );
        let lease = c.acquire("owner".into(), Duration::from_secs(10)).unwrap();
        let rev = c.observe_targets(&device).unwrap().observation.revision;
        (c, device, lease, rev)
    }
    fn tap() -> ElementAction {
        ElementAction::Tap {
            target_id: ID.into(),
        }
    }
    fn effect_report(effect: ObservedEffect) -> TargetEffectReport {
        let raw = raw();
        TargetEffectReport {
            update: TargetEffect {
                token: 1,
                target_id: ID.into(),
                effect,
                region: [0., 0., 1., 1.],
            },
            completed_unix_ms: raw.window.finished_unix_ms,
            proof: Some(std::sync::Arc::new(RawImageObservation {
                connected_usb: raw.connected_usb,
                wda_target: raw.wda_target,
                window: raw.window,
                screenshot: raw.screenshot,
            })),
        }
    }
    #[test]
    fn pending_ack_has_no_effect_authority_and_renews_once_after_proof() {
        let (c, d, l, r) = fixture(Adapter {
            combined: true,
            updates: vec![effect_report(ObservedEffect::Pending)],
            ..Default::default()
        });
        {
            let mut i = c.inner.lock().unwrap();
            i.lease.as_mut().unwrap().renew_on_success = Some(Duration::from_secs(30));
        }
        let before = c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at;
        let result = c.act_element("owner", l, &d, r, tap()).unwrap();
        assert_eq!(result.result.effect, ObservedEffect::Pending);
        assert!(result.next.is_none());
        assert!(c.inner.lock().unwrap().target_last.is_none());
        assert_eq!(
            c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at,
            before
        );
        c.inner.lock().unwrap().adapter.updates = vec![effect_report(ObservedEffect::Changed)];
        assert_eq!(c.target_effect_updates()[0].effect, ObservedEffect::Changed);
        let renewed = c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at;
        assert!(renewed > before);
        c.target_effect_updates();
        assert_eq!(
            c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at,
            renewed
        );
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
    #[test]
    fn deferred_proof_cannot_renew_expired_or_replaced_lease() {
        for replaced in [false, true] {
            let (c, d, l, r) = fixture(Adapter {
                combined: true,
                updates: vec![effect_report(ObservedEffect::Pending)],
                ..Default::default()
            });
            c.act_element("owner", l, &d, r, tap()).unwrap();
            let mut i = c.inner.lock().unwrap();
            i.adapter.updates = vec![effect_report(ObservedEffect::Changed)];
            let lease = i.lease.as_mut().unwrap();
            lease.renew_on_success = Some(Duration::from_secs(30));
            if replaced {
                lease.owner = "other".into();
            } else {
                lease.expires_at = Instant::now() - Duration::from_secs(1);
            }
            let before = lease.expires_at;
            drop(i);
            c.target_effect_updates();
            assert_eq!(
                c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at,
                before
            );
        }
    }
    #[test]
    fn deferred_proof_preserves_identity_window_and_image_checks() {
        for invalid in 0..4 {
            let (c, _, _, _) = fixture(Adapter::default());
            let mut report = effect_report(ObservedEffect::Changed);
            let proof = std::sync::Arc::make_mut(report.proof.as_mut().unwrap());
            match invalid {
                0 => proof.wda_target = HardwareId::new("other").unwrap(),
                1 => proof.screenshot.bytes.clear(),
                2 => proof.window.started_unix_ms = 0,
                _ => report.completed_unix_ms = u64::MAX,
            }
            c.inner.lock().unwrap().adapter.updates = vec![report];
            assert_eq!(
                c.target_effect_updates()[0].effect,
                ObservedEffect::Unconfirmed
            );
        }
    }
    #[test]
    fn already_finished_worker_is_correlated_and_renewed_once() {
        let (c, d, l, r) = fixture(Adapter {
            combined: true,
            updates: vec![effect_report(ObservedEffect::Changed)],
            ..Default::default()
        });
        {
            c.inner
                .lock()
                .unwrap()
                .lease
                .as_mut()
                .unwrap()
                .renew_on_success = Some(Duration::from_secs(30));
        }
        let before = c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at;
        let result = c.act_element("owner", l, &d, r, tap()).unwrap();
        assert_eq!(result.result.effect, ObservedEffect::Changed);
        assert!(result.next.is_none());
        let renewed = c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at;
        assert!(renewed > before);
        c.target_effect_updates();
        assert_eq!(
            c.inner.lock().unwrap().lease.as_ref().unwrap().expires_at,
            renewed
        );
    }
    #[test]
    fn completion_during_collection_is_validated_against_a_later_clock() {
        let (c, _, _, _) = fixture(Adapter {
            fresh_report_on_read: true,
            ..Default::default()
        });
        assert_eq!(c.target_effect_updates()[0].effect, ObservedEffect::Changed);
    }
    #[test]
    fn expected_revision_lease_expiry_and_identity_block_before_send() {
        for invalid in 1..=5 {
            let (c, d, l, r) = fixture(Adapter {
                invalid,
                ..Default::default()
            });
            assert!(c.act_element("owner", l, &d, r, tap()).is_err());
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
        }
        let (c, d, l, r) = fixture(Adapter::default());
        assert!(
            c.act_element("owner", l, &d, Revision(r.0 + 1), tap())
                .is_err()
        );
        assert!(c.act_element("other", l, &d, r, tap()).is_err());
        c.inner.lock().unwrap().lease.as_mut().unwrap().expires_at = Instant::now();
        assert!(c.act_element("owner", l, &d, r, tap()).is_err());
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
    }
    #[test]
    fn uncertain_dispatch_consumes_revision_and_never_replays() {
        let (c, d, l, r) = fixture(Adapter {
            fail_send: true,
            ..Default::default()
        });
        assert!(c.act_element("owner", l, &d, r, tap()).is_err());
        assert!(c.act_element("owner", l, &d, r, tap()).is_err());
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
    #[test]
    fn shared_completion_uses_one_capture_and_keeps_next_on_unverified_effect() {
        for verified in [false, true] {
            let (c, d, l, r) = fixture(Adapter {
                combined: true,
                verified,
                ..Default::default()
            });
            let outcome = c.act_element("owner", l, &d, r, tap()).unwrap();
            assert_eq!(outcome.result.receipt.accepted, Some(true));
            assert_eq!(
                outcome.result.effect,
                if verified {
                    ObservedEffect::Changed
                } else {
                    ObservedEffect::Unconfirmed
                }
            );
            assert!(outcome.next.is_some());
            assert_eq!(c.inner.lock().unwrap().adapter.captures, 2);
            assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
        }
    }
    #[test]
    fn stale_readback_keeps_acknowledgement_and_independent_next_targets() {
        let (c, d, l, r) = fixture(Adapter {
            combined: true,
            verified: true,
            stale_readback: true,
            ..Default::default()
        });
        let outcome = c.act_element("owner", l, &d, r, tap()).unwrap();
        assert_eq!(outcome.result.receipt.accepted, Some(true));
        assert_eq!(outcome.result.effect, ObservedEffect::Unconfirmed);
        assert!(outcome.result.after_image.is_none());
        assert!(outcome.next.is_some());
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
    #[test]
    fn optional_next_capture_failure_preserves_acknowledgement() {
        let (c, d, l, r) = fixture(Adapter::default());
        let outcome = c.act_element("owner", l, &d, r, tap()).unwrap();
        assert_eq!(outcome.result.receipt.accepted, Some(true));
        assert!(outcome.next.is_none());
        assert!(outcome.result.effect == ObservedEffect::Unconfirmed);
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 1);
    }
    #[test]
    fn presentation_ids_and_control_text_are_not_native_handles() {
        assert!(
            !ElementAction::Tap {
                target_id: "e1".into()
            }
            .valid()
        );
        assert!(
            !ElementAction::Text {
                target_id: ID.into(),
                value: "secret\n".into()
            }
            .valid()
        );
        assert!(
            !ElementAction::Text {
                target_id: ID.into(),
                value: String::new()
            }
            .valid()
        );
    }
    #[test]
    fn target_revision_never_admits_legacy_input_and_ordinary_observation_invalidates_it() {
        let (c, d, l, r) = fixture(Adapter::default());
        assert!(matches!(
            c.act(
                "owner",
                l,
                ActionRequest {
                    device: d.clone(),
                    expected_revision: Some(r),
                    action: Action::Tap {
                        at: Point { x: 1.0, y: 1.0 }
                    }
                }
            ),
            Err(ControllerError::StaleRevision)
        ));
        c.observe(&d).unwrap();
        assert!(matches!(
            c.act_element("owner", l, &d, r, tap()),
            Err(ControllerError::StaleRevision)
        ));
        assert_eq!(c.inner.lock().unwrap().adapter.sends, 0);
    }
}
