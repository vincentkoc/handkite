use crate::protocol::{
    Action, ActionRequest, ActionResult, DeviceBinding, DeviceKey, DeviceState, HardwareId,
    LockState, Observation, ObservationWindow, ObservedEffect, Point, Revision, ScreenImage,
    TransportReceipt,
};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct RawObservation {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub accessibility_tree: String,
    pub screenshot: ScreenImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterError;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActionAdmission {
    HomeLaunch { until: Instant },
    VisibleTap { at: Point, until: Instant },
    NativeModalTap { at: Point, until: Instant },
    NativeHome { until: Instant },
    NativeText { until: Instant },
    NativeTextGesture { at: Point, until: Instant },
    NativeButton { until: Instant },
}

pub trait DeviceAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError>;
    fn observe(&mut self) -> Result<RawObservation, AdapterError>;
    /// Private admission bound to the exact action and capture deadline.
    /// Ordinary observations return no admission.
    fn observe_for_action(
        &mut self,
        _action: &Action,
    ) -> Result<(RawObservation, Option<ActionAdmission>), AdapterError> {
        self.observe().map(|observation| (observation, None))
    }
    /// The adapter must send no action bytes at or after this monotonic deadline.
    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError>;
    /// Receives only validated private admission used by this action check.
    /// Native admissions require an additional native unlock check before sending.
    fn execute_with_admission(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        admission: Option<ActionAdmission>,
    ) -> Result<TransportReceipt, AdapterError> {
        if matches!(
            admission,
            Some(
                ActionAdmission::NativeHome { .. }
                    | ActionAdmission::NativeModalTap { .. }
                    | ActionAdmission::NativeText { .. }
                    | ActionAdmission::NativeTextGesture { .. }
                    | ActionAdmission::NativeButton { .. }
            )
        ) {
            return Err(AdapterError);
        }
        self.execute(action, send_deadline)
    }
    /// Query only the pinned device, without sending input or retrying the action.
    fn native_lock_state(&mut self) -> LockState {
        LockState::Uncertain
    }
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LeaseId(u128);

impl std::fmt::Debug for LeaseId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LeaseId([redacted])")
    }
}

impl TryFrom<String> for LeaseId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("invalid lease token");
        }
        u128::from_str_radix(&value, 16)
            .map(Self)
            .map_err(|_| "invalid lease token")
    }
}

impl From<LeaseId> for String {
    fn from(value: LeaseId) -> Self {
        format!("{:032x}", value.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewChange {
    pub state: bool,
    pub accessibility_tree: bool,
    pub screenshot: bool,
}

impl ViewChange {
    fn between(before: &Observation, after: &Observation) -> Self {
        Self {
            state: before.state != after.state,
            accessibility_tree: before.accessibility_tree != after.accessibility_tree,
            screenshot: before.screenshot != after.screenshot,
        }
    }

    pub fn any(self) -> bool {
        self.state || self.accessibility_tree || self.screenshot
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerError {
    Busy,
    InvalidLease,
    Randomness,
    DeviceMismatch,
    StaleRevision,
    ViewChanged(ViewChange),
    LockedAction,
    StaleObservation,
    InvalidObservationWindow,
    RevisionExhausted,
    Adapter,
    Poisoned,
}

struct Lease {
    id: LeaseId,
    owner: String,
    expires_at: Instant,
}

struct Inner<A> {
    adapter: A,
    last: Option<Observation>,
    revision: Revision,
    lease: Option<Lease>,
}

/// Serializes all device access. Authentication of the caller precedes this controller.
pub struct Controller<A> {
    binding: DeviceBinding,
    max_window_ms: u64,
    max_age_ms: u64,
    inner: Mutex<Inner<A>>,
}

impl<A: DeviceAdapter> Controller<A> {
    pub fn new(binding: DeviceBinding, adapter: A, max_window_ms: u64, max_age_ms: u64) -> Self {
        Self {
            binding,
            max_window_ms,
            max_age_ms,
            inner: Mutex::new(Inner {
                adapter,
                last: None,
                revision: Revision(0),
                lease: None,
            }),
        }
    }

    pub fn acquire(&self, owner: String, ttl: Duration) -> Result<LeaseId, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if owner.is_empty() || ttl.is_zero() || ttl > Duration::from_secs(300) {
            return Err(ControllerError::InvalidLease);
        }
        if inner
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > Instant::now())
        {
            return Err(ControllerError::Busy);
        }
        let mut token = [0_u8; 16];
        getrandom::fill(&mut token).map_err(|_| ControllerError::Randomness)?;
        let id = LeaseId(u128::from_be_bytes(token));
        inner.lease = Some(Lease {
            id,
            owner,
            expires_at: Instant::now() + ttl,
        });
        Ok(id)
    }

    pub fn release(&self, owner: &str, id: LeaseId) -> Result<(), ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&inner.lease, Some(lease) if lease.id == id && lease.owner == owner) {
            return Err(ControllerError::InvalidLease);
        }
        inner.lease = None;
        Ok(())
    }

    pub fn observe(&self, requested: &DeviceKey) -> Result<Observation, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        self.observe_inner(&mut inner, requested)
    }

    pub fn act(
        &self,
        owner: &str,
        lease_id: LeaseId,
        request: ActionRequest,
    ) -> Result<ActionResult, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&inner.lease, Some(lease) if lease.id == lease_id && lease.owner == owner && lease.expires_at > Instant::now())
        {
            return Err(ControllerError::InvalidLease);
        }
        if request.device != self.binding.key {
            return Err(ControllerError::DeviceMismatch);
        }
        if let Some(expected) = request.expected_revision
            && inner.last.as_ref().map(|observation| observation.revision) != Some(expected)
        {
            return Err(ControllerError::StaleRevision);
        }
        let (usb, wda) = inner
            .adapter
            .identities()
            .map_err(|_| ControllerError::Adapter)?;
        self.binding
            .verify(&request.device, &usb, &wda)
            .map_err(|_| ControllerError::DeviceMismatch)?;

        let (fresh, admission) =
            self.capture(&mut inner, &request.device, Some(&request.action))?;
        let home_launch_until = match admission {
            Some(ActionAdmission::HomeLaunch { until }) => Some(until),
            _ => None,
        };
        let admitted_home_launch = matches!(&request.action, Action::LaunchApp { .. })
            && fresh.state.lock == LockState::Uncertain
            && home_launch_until.is_some_and(|until| Instant::now() < until);
        let admitted_modal_tap = matches!(
            (&request.action, admission),
            (Action::Tap { at }, Some(ActionAdmission::NativeModalTap { at: admitted, until }))
                if *at == admitted
                    && fresh.state.lock == LockState::Uncertain
                    && fresh.state.wda_ready
                    && fresh.state.foreground_app.as_deref() == Some("com.apple.springboard")
                    && Instant::now() < until
        );
        let pixel_drift_until = match (&request.action, admission) {
            (_, Some(ActionAdmission::NativeModalTap { until, .. })) if admitted_modal_tap => {
                Some(until)
            }
            (
                Action::Tap { at },
                Some(ActionAdmission::VisibleTap {
                    at: admitted,
                    until,
                }),
            ) if *at == admitted
                && fresh.state.lock == LockState::Unlocked
                && fresh.state.wda_ready
                && Instant::now() < until =>
            {
                Some(until)
            }
            (Action::Home, Some(ActionAdmission::NativeHome { until }))
                if fresh.state.lock == LockState::Unlocked
                    && fresh.state.wda_ready
                    && Instant::now() < until =>
            {
                Some(until)
            }
            (Action::Text { .. }, Some(ActionAdmission::NativeText { until }))
                if fresh.state.lock == LockState::Unlocked
                    && fresh.state.wda_ready
                    && Instant::now() < until =>
            {
                Some(until)
            }
            (Action::ClickButton { .. }, Some(ActionAdmission::NativeButton { until }))
                if fresh.state.lock == LockState::Unlocked
                    && fresh.state.wda_ready
                    && Instant::now() < until =>
            {
                Some(until)
            }
            (
                Action::DoubleTap { at } | Action::LongPress { at, .. },
                Some(ActionAdmission::NativeTextGesture {
                    at: admitted,
                    until,
                }),
            ) if *at == admitted
                && fresh.state.lock == LockState::Unlocked
                && fresh.state.wda_ready
                && Instant::now() < until =>
            {
                Some(until)
            }
            _ => None,
        };
        // App launch uses a bundle ID, so Home animations need not match.
        // Fresh private admission and device state still must.
        if request.expected_revision.is_some()
            && inner.last.as_ref().is_some_and(|last| {
                !(same_view(last, &fresh)
                    || admitted_home_launch && last.state == fresh.state
                    || pixel_drift_until.is_some()
                        && last.state == fresh.state
                        && last.accessibility_tree == fresh.accessibility_tree)
            })
        {
            let change =
                ViewChange::between(inner.last.as_ref().expect("checked revision"), &fresh);
            inner.last = Some(fresh);
            return Err(ControllerError::ViewChanged(change));
        }
        if !matches!(&request.action, Action::Home)
            && fresh.state.lock != LockState::Unlocked
            && !admitted_home_launch
            && !admitted_modal_tap
        {
            inner.last = Some(fresh);
            return Err(ControllerError::LockedAction);
        }
        let is_lock = matches!(&request.action, Action::Lock);
        let finished_unix_ms = fresh.window.finished_unix_ms;
        inner.last = Some(fresh);
        let Some(lease) = inner
            .lease
            .as_ref()
            .filter(|lease| lease.expires_at > Instant::now())
        else {
            return Err(ControllerError::InvalidLease);
        };
        let observed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::StaleObservation)?
            .as_millis() as u64;
        let age = observed_at
            .checked_sub(finished_unix_ms)
            .ok_or(ControllerError::StaleObservation)?;
        let remaining = self
            .max_age_ms
            .checked_sub(age)
            .filter(|remaining| *remaining > 0)
            .ok_or(ControllerError::StaleObservation)?;
        let observation_deadline = Instant::now()
            .checked_add(Duration::from_millis(remaining))
            .ok_or(ControllerError::StaleObservation)?;
        let mut send_deadline = observation_deadline.min(lease.expires_at);
        if admitted_home_launch {
            send_deadline = send_deadline.min(home_launch_until.expect("admitted deadline"));
        }
        if let Some(until) = pixel_drift_until {
            send_deadline = send_deadline.min(until);
        }
        let execution_admission = (admitted_home_launch || pixel_drift_until.is_some())
            .then_some(admission)
            .flatten();
        let receipt = match inner.adapter.execute_with_admission(
            &request.action,
            send_deadline,
            execution_admission,
        ) {
            Ok(receipt) => receipt,
            // An error can follow delivery. Keep checking the postcondition without replay.
            Err(_) if is_lock => TransportReceipt {
                accepted: None,
                action_kind: request.action.kind().into(),
            },
            Err(_) => {
                inner.last = None;
                return Err(ControllerError::Adapter);
            }
        };
        let native_lock = is_lock.then(|| inner.adapter.native_lock_state());
        let lock_effect = if native_lock == Some(LockState::Locked) {
            ObservedEffect::LockConfirmed
        } else {
            ObservedEffect::Unconfirmed
        };
        let observed = self
            .capture(&mut inner, &request.device, None)
            .map(|(observation, _)| observation);
        match observed {
            Ok(after) => {
                let changed = inner
                    .last
                    .as_ref()
                    .is_some_and(|before| !same_view(before, &after));
                let effect = if is_lock {
                    lock_effect
                } else if receipt.accepted == Some(true) && changed {
                    ObservedEffect::Changed
                } else {
                    ObservedEffect::Unconfirmed
                };
                inner.last = Some(after.clone());
                Ok(ActionResult {
                    receipt,
                    after: Some(after),
                    native_lock,
                    effect,
                })
            }
            Err(ControllerError::DeviceMismatch) => {
                inner.last = None;
                Err(ControllerError::DeviceMismatch)
            }
            Err(_) => {
                inner.last = None;
                Ok(ActionResult {
                    receipt,
                    after: None,
                    native_lock,
                    effect: lock_effect,
                })
            }
        }
    }

    fn observe_inner(
        &self,
        inner: &mut Inner<A>,
        requested: &DeviceKey,
    ) -> Result<Observation, ControllerError> {
        let (observation, _) = self.capture(inner, requested, None)?;
        inner.last = Some(observation.clone());
        Ok(observation)
    }

    fn capture(
        &self,
        inner: &mut Inner<A>,
        requested: &DeviceKey,
        action: Option<&Action>,
    ) -> Result<(Observation, Option<ActionAdmission>), ControllerError> {
        let (raw, admission) = match action {
            Some(action) => inner.adapter.observe_for_action(action),
            None => inner
                .adapter
                .observe()
                .map(|observation| (observation, None)),
        }
        .map_err(|_| ControllerError::Adapter)?;
        self.binding
            .verify(requested, &raw.connected_usb, &raw.wda_target)
            .map_err(|_| ControllerError::DeviceMismatch)?;
        let duration = raw
            .window
            .duration_ms()
            .map_err(|_| ControllerError::InvalidObservationWindow)?;
        if duration > self.max_window_ms {
            return Err(ControllerError::InvalidObservationWindow);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ControllerError::StaleObservation)?
            .as_millis() as u64;
        let age = now
            .checked_sub(raw.window.finished_unix_ms)
            .ok_or(ControllerError::StaleObservation)?;
        if age > self.max_age_ms {
            return Err(ControllerError::StaleObservation);
        }
        let revision = inner
            .revision
            .next()
            .map_err(|_| ControllerError::RevisionExhausted)?;
        inner.revision = revision;
        Ok((
            Observation {
                device: requested.clone(),
                revision,
                window: raw.window,
                state: raw.state,
                accessibility_tree: raw.accessibility_tree,
                screenshot: raw.screenshot,
            },
            admission,
        ))
    }
}

fn same_view(before: &Observation, after: &Observation) -> bool {
    before.state == after.state
        && before.accessibility_tree == after.accessibility_tree
        && before.screenshot == after.screenshot
}

#[cfg(test)]
mod tests;
