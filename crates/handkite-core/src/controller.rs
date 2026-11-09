use crate::protocol::{
    Action, ActionRequest, ActionResult, DeviceBinding, DeviceKey, DeviceState, HardwareId,
    LockState, Observation, ObservationWindow, ObservedEffect, Revision, ScreenImage,
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

pub trait DeviceAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError>;
    fn observe(&mut self) -> Result<RawObservation, AdapterError>;
    fn execute(&mut self, action: &Action) -> Result<TransportReceipt, AdapterError>;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerError {
    Busy,
    InvalidLease,
    Randomness,
    DeviceMismatch,
    StaleRevision,
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
        if let Some(expected) = request.expected_revision {
            if inner.last.as_ref().map(|observation| observation.revision) != Some(expected) {
                return Err(ControllerError::StaleRevision);
            }
        }
        let (usb, wda) = inner
            .adapter
            .identities()
            .map_err(|_| ControllerError::Adapter)?;
        self.binding
            .verify(&request.device, &usb, &wda)
            .map_err(|_| ControllerError::DeviceMismatch)?;

        let fresh = self.capture(&mut inner, &request.device)?;
        if request.expected_revision.is_some()
            && inner
                .last
                .as_ref()
                .is_some_and(|last| !same_view(last, &fresh))
        {
            inner.last = Some(fresh);
            return Err(ControllerError::StaleRevision);
        }
        if !matches!(&request.action, Action::Home) && fresh.state.lock != LockState::Unlocked {
            inner.last = Some(fresh);
            return Err(ControllerError::LockedAction);
        }
        inner.last = Some(fresh);

        let is_lock = matches!(&request.action, Action::Lock);
        if inner
            .lease
            .as_ref()
            .is_none_or(|lease| lease.expires_at <= Instant::now())
        {
            return Err(ControllerError::InvalidLease);
        }
        let receipt = match inner.adapter.execute(&request.action) {
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
        let observed = self.capture(&mut inner, &request.device);
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
        let observation = self.capture(inner, requested)?;
        inner.last = Some(observation.clone());
        Ok(observation)
    }

    fn capture(
        &self,
        inner: &mut Inner<A>,
        requested: &DeviceKey,
    ) -> Result<Observation, ControllerError> {
        let raw = inner
            .adapter
            .observe()
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
        Ok(Observation {
            device: requested.clone(),
            revision,
            window: raw.window,
            state: raw.state,
            accessibility_tree: raw.accessibility_tree,
            screenshot: raw.screenshot,
        })
    }
}

fn same_view(before: &Observation, after: &Observation) -> bool {
    before.state == after.state
        && before.accessibility_tree == after.accessibility_tree
        && before.screenshot == after.screenshot
}

#[cfg(test)]
mod tests;
