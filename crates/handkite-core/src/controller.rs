use crate::protocol::{
    Action, ActionRequest, ActionResult, CaptureMode, DeviceBinding, DeviceKey, DeviceState,
    HardwareId, ImageObservation, LockState, Observation, ObservationWindow, ObservedEffect, Point,
    Revision, ScreenImage, TransportReceipt,
};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod focused_text;
pub use focused_text::{PreparedText, TextReadback};

#[derive(Clone)]
pub struct RawObservation {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub accessibility_tree: String,
    pub screenshot: ScreenImage,
}

pub struct RawImageObservation {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub window: ObservationWindow,
    pub screenshot: ScreenImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterError;

/// Fixed categories only: adapter details can contain private device content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterDiagnostic {
    IdentityCheck,
    PreActionCapture,
    OwnedBefore,
    NativeBefore,
    ButtonAdmission,
    ButtonResolve,
    ButtonClick,
    SessionStatus,
    SessionCheck,
    InputExpiredBeforeSend,
    InputConnect,
    InputInvalidRequest,
    InputAfterSend,
    InputTimedOutAfterSend,
    InputResponseRejected,
    OwnershipAfterAck,
}

impl AdapterDiagnostic {
    pub const ALL: [Self; 16] = [
        Self::IdentityCheck,
        Self::PreActionCapture,
        Self::OwnedBefore,
        Self::NativeBefore,
        Self::ButtonAdmission,
        Self::ButtonResolve,
        Self::ButtonClick,
        Self::SessionStatus,
        Self::SessionCheck,
        Self::InputExpiredBeforeSend,
        Self::InputConnect,
        Self::InputInvalidRequest,
        Self::InputAfterSend,
        Self::InputTimedOutAfterSend,
        Self::InputResponseRejected,
        Self::OwnershipAfterAck,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::IdentityCheck => "AdapterIdentityCheck",
            Self::PreActionCapture => "AdapterPreActionCapture",
            Self::OwnedBefore => "AdapterOwnedBefore",
            Self::NativeBefore => "AdapterNativeBefore",
            Self::ButtonAdmission => "AdapterButtonAdmission",
            Self::ButtonResolve => "AdapterButtonResolve",
            Self::ButtonClick => "AdapterButtonClick",
            Self::SessionStatus => "AdapterSessionStatus",
            Self::SessionCheck => "AdapterSessionCheck",
            Self::InputExpiredBeforeSend => "AdapterInputExpiredBeforeSend",
            Self::InputConnect => "AdapterInputConnect",
            Self::InputInvalidRequest => "AdapterInputInvalidRequest",
            Self::InputAfterSend => "AdapterInputAfterSend",
            Self::InputTimedOutAfterSend => "AdapterInputTimedOutAfterSend",
            Self::InputResponseRejected => "AdapterInputResponseRejected",
            Self::OwnershipAfterAck => "AdapterOwnershipAfterAck",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActionAdmission {
    HomeLaunch {
        until: Instant,
    },
    VisibleTap {
        at: Point,
        until: Instant,
    },
    NativeModalTap {
        at: Point,
        until: Instant,
    },
    NativeHome {
        until: Instant,
    },
    NativeAppLaunch {
        until: Instant,
    },
    NativeSwipe {
        from: Point,
        to: Point,
        duration_ms: u32,
        until: Instant,
    },
    NativeText {
        until: Instant,
    },
    NativeTextGesture {
        at: Point,
        until: Instant,
    },
    NativeButton {
        until: Instant,
    },
    NativeKey {
        key: crate::protocol::KeyboardKey,
        until: Instant,
    },
}

pub trait DeviceAdapter {
    /// Consume the fixed diagnostic from the immediately preceding failure.
    fn take_error_diagnostic(&mut self) -> Option<AdapterDiagnostic> {
        None
    }

    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError>;
    fn observe(&mut self) -> Result<RawObservation, AdapterError>;
    fn observe_image(&mut self) -> Result<RawImageObservation, AdapterError> {
        Err(AdapterError)
    }
    /// Owned adapters can forbid generic text fallback after failed preparation.
    fn requires_current_text_preparation(&self) -> bool {
        false
    }

    /// Optional current-selection preparation. No input may be sent here.
    /// A strict adapter's `None` permits one read-only refresh, never generic input.
    fn prepare_current_text(
        &mut self,
        _action: &Action,
        _baseline: &Observation,
    ) -> Result<Option<PreparedText>, AdapterError> {
        Ok(None)
    }
    /// Consume one prepared token, recheck focus and native unlock, then send once.
    fn execute_prepared_text(
        &mut self,
        _action: &Action,
        _token: u64,
        _send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        Err(AdapterError)
    }
    /// Read the same field after input. Failure must never resend the action.
    fn observe_after_prepared_text(&mut self, _token: u64) -> Result<TextReadback, AdapterError> {
        Err(AdapterError)
    }
    /// Private admission bound to the exact action and capture deadline.
    /// Ordinary observations return no admission.
    fn observe_for_action(
        &mut self,
        _action: &Action,
    ) -> Result<(RawObservation, Option<ActionAdmission>), AdapterError> {
        self.observe().map(|observation| (observation, None))
    }
    /// Capture the result without resending the acknowledged action.
    fn observe_after_action(
        &mut self,
        _action: &Action,
        _admission: Option<ActionAdmission>,
        _receipt: &TransportReceipt,
    ) -> Result<RawObservation, AdapterError> {
        self.observe()
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
                    | ActionAdmission::NativeAppLaunch { .. }
                    | ActionAdmission::NativeSwipe { .. }
                    | ActionAdmission::NativeModalTap { .. }
                    | ActionAdmission::NativeText { .. }
                    | ActionAdmission::NativeTextGesture { .. }
                    | ActionAdmission::NativeButton { .. }
                    | ActionAdmission::NativeKey { .. }
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
#[serde(rename_all = "snake_case")]
pub enum GestureAdmission {
    Missing,
    Expired,
    Mismatched,
    Present,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewChange {
    pub state: bool,
    pub accessibility_tree: bool,
    pub screenshot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gesture_admission: Option<GestureAdmission>,
}

impl ViewChange {
    fn between(before: &Observation, after: &Observation) -> Self {
        Self {
            state: before.state != after.state,
            accessibility_tree: before.accessibility_tree != after.accessibility_tree,
            screenshot: before.screenshot != after.screenshot,
            gesture_admission: None,
        }
    }

    pub fn any(self) -> bool {
        self.state || self.accessibility_tree || self.screenshot
    }
}

fn gesture_admission_status(
    action: &Action,
    admission: Option<ActionAdmission>,
    now: Instant,
) -> Option<GestureAdmission> {
    if let Action::Swipe {
        from,
        to,
        duration_ms,
    } = action
    {
        return Some(match admission {
            Some(ActionAdmission::NativeSwipe {
                from: admitted_from,
                to: admitted_to,
                duration_ms: admitted_duration,
                ..
            }) if *from != admitted_from
                || *to != admitted_to
                || *duration_ms != admitted_duration =>
            {
                GestureAdmission::Mismatched
            }
            Some(ActionAdmission::NativeSwipe { until, .. }) if now >= until => {
                GestureAdmission::Expired
            }
            Some(ActionAdmission::NativeSwipe { .. }) => GestureAdmission::Present,
            _ => GestureAdmission::Missing,
        });
    }
    let (Action::DoubleTap { at } | Action::LongPress { at, .. }) = action else {
        return None;
    };
    Some(match admission {
        Some(ActionAdmission::NativeTextGesture { at: admitted, .. }) if *at != admitted => {
            GestureAdmission::Mismatched
        }
        Some(ActionAdmission::NativeTextGesture { until, .. }) if now >= until => {
            GestureAdmission::Expired
        }
        Some(ActionAdmission::NativeTextGesture { .. }) => GestureAdmission::Present,
        _ => GestureAdmission::Missing,
    })
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
    AdapterAt(AdapterDiagnostic),
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
        self.act_with_capture(owner, lease_id, request, CaptureMode::Paired)
    }

    pub fn act_with_capture(
        &self,
        owner: &str,
        lease_id: LeaseId,
        request: ActionRequest,
        capture: CaptureMode,
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
            .map_err(|_| Self::adapter_error(&mut inner))?;
        self.binding
            .verify(&request.device, &usb, &wda)
            .map_err(|_| ControllerError::DeviceMismatch)?;

        if let Some(result) = self.try_focused_text(&mut inner, &request, capture)? {
            return Ok(result);
        }

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
            (Action::LaunchApp { .. }, Some(ActionAdmission::NativeAppLaunch { until }))
                if fresh.state.lock == LockState::Unlocked
                    && fresh.state.wda_ready
                    && Instant::now() < until =>
            {
                Some(until)
            }
            (
                Action::Swipe {
                    from,
                    to,
                    duration_ms,
                },
                Some(ActionAdmission::NativeSwipe {
                    from: admitted_from,
                    to: admitted_to,
                    duration_ms: admitted_duration,
                    until,
                }),
            ) if *from == admitted_from
                && *to == admitted_to
                && *duration_ms == admitted_duration
                && fresh.state.lock == LockState::Unlocked
                && fresh.state.wda_ready
                && Instant::now() < until =>
            {
                Some(until)
            }
            (
                Action::Text { .. } | Action::TextCurrent { .. },
                Some(ActionAdmission::NativeText { until }),
            ) if fresh.state.lock == LockState::Unlocked
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
                Action::KeyCurrent { key },
                Some(ActionAdmission::NativeKey {
                    key: admitted,
                    until,
                }),
            ) if *key == admitted
                && fresh.state.lock == LockState::Unlocked
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
            let mut change =
                ViewChange::between(inner.last.as_ref().expect("checked revision"), &fresh);
            change.gesture_admission =
                gesture_admission_status(&request.action, admission, Instant::now());
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
            Err(_) if is_lock => {
                inner.adapter.take_error_diagnostic();
                TransportReceipt {
                    accepted: None,
                    action_kind: request.action.kind().into(),
                }
            }
            Err(_) => {
                inner.last = None;
                return Err(Self::adapter_error(&mut inner));
            }
        };
        let native_lock = is_lock.then(|| inner.adapter.native_lock_state());
        let lock_effect = if native_lock == Some(LockState::Locked) {
            ObservedEffect::LockConfirmed
        } else {
            ObservedEffect::Unconfirmed
        };
        if capture == CaptureMode::ImageOnly
            && !matches!(request.action, Action::Home | Action::Lock)
        {
            let before = inner.last.take().map(|observation| observation.screenshot);
            let observed = inner
                .adapter
                .observe_image()
                .map_err(|_| ControllerError::Adapter)
                .and_then(|raw| self.accept_image(&request.device, raw));
            let after_image = match observed {
                Ok(image) => Some(image),
                Err(ControllerError::DeviceMismatch) => {
                    return Err(ControllerError::DeviceMismatch);
                }
                Err(_) => None,
            };
            let changed = after_image
                .as_ref()
                .zip(before.as_ref())
                .is_some_and(|(after, before)| after.screenshot != *before);
            return Ok(ActionResult {
                receipt: receipt.clone(),
                after: None,
                after_image,
                native_lock,
                effect: if receipt.accepted == Some(true) && changed {
                    ObservedEffect::Changed
                } else {
                    ObservedEffect::Unconfirmed
                },
            });
        }
        let observed = inner
            .adapter
            .observe_after_action(&request.action, execution_admission, &receipt)
            .map_err(|_| ControllerError::Adapter)
            .and_then(|raw| self.accept_capture(&mut inner, &request.device, raw));
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
                    after_image: None,
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
                    after_image: None,
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

    pub fn observe_image(
        &self,
        requested: &DeviceKey,
    ) -> Result<ImageObservation, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        inner.last = None;
        let raw = inner
            .adapter
            .observe_image()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        self.accept_image(requested, raw)
    }

    fn accept_image(
        &self,
        requested: &DeviceKey,
        raw: RawImageObservation,
    ) -> Result<ImageObservation, ControllerError> {
        self.binding
            .verify(requested, &raw.connected_usb, &raw.wda_target)
            .map_err(|_| ControllerError::DeviceMismatch)?;
        self.validate_window(&raw.window)?;
        if raw.screenshot.bytes.is_empty() {
            return Err(ControllerError::Adapter);
        }
        Ok(ImageObservation {
            device: requested.clone(),
            window: raw.window,
            screenshot: raw.screenshot,
        })
    }

    fn validate_window(&self, window: &ObservationWindow) -> Result<(), ControllerError> {
        let duration = window
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
            .checked_sub(window.finished_unix_ms)
            .ok_or(ControllerError::StaleObservation)?;
        if age > self.max_age_ms {
            return Err(ControllerError::StaleObservation);
        }
        Ok(())
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
        .map_err(|_| Self::adapter_error(inner))?;
        self.accept_capture(inner, requested, raw)
            .map(|observation| (observation, admission))
    }

    fn adapter_error(inner: &mut Inner<A>) -> ControllerError {
        inner
            .adapter
            .take_error_diagnostic()
            .map(ControllerError::AdapterAt)
            .unwrap_or(ControllerError::Adapter)
    }

    fn accept_capture(
        &self,
        inner: &mut Inner<A>,
        requested: &DeviceKey,
        raw: RawObservation,
    ) -> Result<Observation, ControllerError> {
        self.binding
            .verify(requested, &raw.connected_usb, &raw.wda_target)
            .map_err(|_| ControllerError::DeviceMismatch)?;
        self.validate_window(&raw.window)?;
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
