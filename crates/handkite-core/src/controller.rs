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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct LeaseId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerError {
    Busy,
    InvalidLease,
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
    next_lease: u64,
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
                next_lease: 0,
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
        inner.next_lease = inner
            .next_lease
            .checked_add(1)
            .ok_or(ControllerError::InvalidLease)?;
        let id = LeaseId(inner.next_lease);
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

        let receipt = match inner.adapter.execute(&request.action) {
            Ok(receipt) => receipt,
            Err(_) => {
                inner.last = None;
                return Err(ControllerError::Adapter);
            }
        };
        let observed = self.capture(&mut inner, &request.device);
        match observed {
            Ok(after) => {
                let changed = inner
                    .last
                    .as_ref()
                    .is_some_and(|before| !same_view(before, &after));
                let effect = if receipt.accepted && changed {
                    ObservedEffect::Changed
                } else {
                    ObservedEffect::Unconfirmed
                };
                inner.last = Some(after.clone());
                Ok(ActionResult {
                    receipt,
                    after: Some(after),
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
                    effect: ObservedEffect::Unconfirmed,
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
