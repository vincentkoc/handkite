use crate::protocol::{
    Action, ActionRequest, ActionResult, CaptureMode, DeviceBinding, DeviceKey, DeviceState,
    HardwareId, ImageObservation, LockState, Motion, Observation, ObservationWindow,
    ObservedEffect, Point, Revision, ScreenImage, TransportReceipt,
};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod focused_text;
mod navigation;
mod targets;
pub use focused_text::{PreparedText, TextReadback};
pub use navigation::{NavigationObservation, NavigationResult};
pub use targets::{
    ElementAction, ElementTarget, PreparedTarget, TargetActionResult, TargetCompletion,
    TargetEffect, TargetEffectReport, TargetObservation,
};

#[derive(Clone)]
pub struct RawObservation {
    pub connected_usb: HardwareId,
    pub wda_target: HardwareId,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub accessibility_tree: String,
    pub screenshot: ScreenImage,
}

#[derive(Clone)]
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
    CaptureImageDrift,
    CaptureHome,
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
    TargetCapture,
    TargetHome,
    TargetCandidates,
    TargetMapping,
    TargetOcr,
    TargetSettings,
    TargetPrepare,
    TargetRevalidation,
}

impl AdapterDiagnostic {
    pub const ALL: [Self; 26] = [
        Self::IdentityCheck,
        Self::CaptureImageDrift,
        Self::CaptureHome,
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
        Self::TargetCapture,
        Self::TargetHome,
        Self::TargetCandidates,
        Self::TargetMapping,
        Self::TargetOcr,
        Self::TargetSettings,
        Self::TargetPrepare,
        Self::TargetRevalidation,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::IdentityCheck => "AdapterIdentityCheck",
            Self::CaptureImageDrift => "AdapterCaptureImageDrift",
            Self::CaptureHome => "AdapterCaptureHome",
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
            Self::TargetCapture => "AdapterTargetCapture",
            Self::TargetHome => "AdapterTargetHome",
            Self::TargetCandidates => "AdapterTargetCandidates",
            Self::TargetMapping => "AdapterTargetMapping",
            Self::TargetOcr => "AdapterTargetOcr",
            Self::TargetSettings => "AdapterTargetSettings",
            Self::TargetPrepare => "AdapterTargetPrepare",
            Self::TargetRevalidation => "AdapterTargetRevalidation",
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
    NativeLock {
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
    /// Image plus fresh native lock, foreground and ownership evidence. No XML authority.
    fn observe_navigation(&mut self) -> Result<RawObservation, AdapterError> {
        Err(AdapterError)
    }
    /// Readback within one acknowledged Home operation. A retry may carry
    /// only a just-completed proof; adapters without that support read anew.
    fn observe_home_readback(&mut self, _retry: bool) -> Result<RawObservation, AdapterError> {
        self.observe_navigation()
    }
    fn prepare_navigation(
        &mut self,
        _action: &Action,
    ) -> Result<(RawObservation, ActionAdmission), AdapterError> {
        Err(AdapterError)
    }
    fn execute_navigation(
        &mut self,
        _action: &Action,
        _deadline: Instant,
        _admission: ActionAdmission,
        _expected_app: &str,
    ) -> Result<TransportReceipt, AdapterError> {
        Err(AdapterError)
    }
    /// Consume the fixed diagnostic from the immediately preceding failure.
    fn take_error_diagnostic(&mut self) -> Option<AdapterDiagnostic> {
        None
    }

    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError>;
    fn settle_target_effects(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
    /// Settle before one element action. Adapters may defer proofs about
    /// regions the target does not overlap; the default drains everything.
    fn settle_target_effects_for(&mut self, _target_id: &str) -> Result<(), AdapterError> {
        self.settle_target_effects()
    }
    fn target_effect_updates(&mut self) -> Vec<TargetEffectReport> {
        Vec::new()
    }
    fn observe(&mut self) -> Result<RawObservation, AdapterError>;
    fn observe_targets(&mut self) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
        Err(AdapterError)
    }

    fn observe_targets_since(
        &mut self,
        _revision: Option<Revision>,
    ) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
        self.observe_targets()
    }

    fn bind_targets(&mut self, _revision: Revision) -> Result<(), AdapterError> {
        Err(AdapterError)
    }

    fn prepare_target(
        &mut self,
        _revision: Revision,
        _action: &ElementAction,
        _baseline: &Observation,
    ) -> Result<PreparedTarget, AdapterError> {
        Err(AdapterError)
    }

    fn execute_target(
        &mut self,
        _action: &ElementAction,
        _token: u64,
        _deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        Err(AdapterError)
    }

    fn target_readback(&mut self, _token: u64) -> Result<TextReadback, AdapterError> {
        Err(AdapterError)
    }
    /// One post-input capture may provide both proof and the next targets.
    /// Neither a missing proof nor a failed next capture permits replay.
    fn complete_target(&mut self, token: u64) -> TargetCompletion {
        let readback = self.target_readback(token);
        let next = self.observe_targets();
        TargetCompletion { readback, next }
    }

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
    /// Like `execute_with_admission`, with optional motion shaping for this one
    /// send. Adapters that cannot shape motion ignore it.
    fn execute_with_motion(
        &mut self,
        action: &Action,
        send_deadline: Instant,
        admission: Option<ActionAdmission>,
        _motion: Option<Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_with_admission(action, send_deadline, admission)
    }
    /// Like `execute_prepared_text`, with optional typing-cadence shaping.
    fn execute_prepared_text_with_motion(
        &mut self,
        action: &Action,
        token: u64,
        send_deadline: Instant,
        _motion: Option<Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        self.execute_prepared_text(action, token, send_deadline)
    }
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
                    | ActionAdmission::NativeLock { .. }
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
    renew_on_success: Option<Duration>,
}

fn renew_verified_action(
    lease: &mut Option<Lease>,
    owner: &str,
    id: LeaseId,
    result: &ActionResult,
    now: Instant,
) {
    if result.receipt.accepted != Some(true)
        || !matches!(
            result.effect,
            ObservedEffect::Changed | ObservedEffect::LockConfirmed
        )
    {
        return;
    }
    if let Some(lease) = lease
        && lease.id == id
        && lease.owner == owner
        && lease.expires_at > now
        && let Some(ttl) = lease.renew_on_success
    {
        lease.expires_at = now + ttl;
    }
}

struct Inner<A> {
    adapter: A,
    last: Option<Observation>,
    target_last: Option<Observation>,
    navigation_last: Option<NavigationObservation>,
    revision: Revision,
    lease: Option<Lease>,
    pending_target_lease: Option<(String, LeaseId, String, u64)>,
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
                target_last: None,
                navigation_last: None,
                revision: Revision(0),
                lease: None,
                pending_target_lease: None,
            }),
        }
    }

    pub fn acquire(&self, owner: String, ttl: Duration) -> Result<LeaseId, ControllerError> {
        self.acquire_with_renewal(owner, ttl, false)
    }

    /// Renew only a still-live lease after verified input, never after observation or refusal.
    pub fn acquire_with_renewal(
        &self,
        owner: String,
        ttl: Duration,
        renew_on_success: bool,
    ) -> Result<LeaseId, ControllerError> {
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
        inner
            .adapter
            .settle_target_effects()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        inner.pending_target_lease = None;
        getrandom::fill(&mut token).map_err(|_| ControllerError::Randomness)?;
        let id = LeaseId(u128::from_be_bytes(token));
        inner.lease = Some(Lease {
            id,
            owner,
            expires_at: Instant::now() + ttl,
            renew_on_success: renew_on_success.then_some(ttl),
        });
        Ok(id)
    }

    pub fn release(&self, owner: &str, id: LeaseId) -> Result<(), ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        if !matches!(&inner.lease, Some(lease) if lease.id == id && lease.owner == owner) {
            return Err(ControllerError::InvalidLease);
        }
        inner
            .adapter
            .settle_target_effects()
            .map_err(|_| Self::adapter_error(&mut inner))?;
        inner.lease = None;
        inner.pending_target_lease = None;
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
        self.act_with_motion(owner, lease_id, request, capture, None)
    }

    /// As `act_with_capture`, passing opt-in motion shaping to the one send.
    /// Every lease, revision, identity, admission and verification check is
    /// identical with or without `motion`.
    pub fn act_with_motion(
        &self,
        owner: &str,
        lease_id: LeaseId,
        request: ActionRequest,
        capture: CaptureMode,
        motion: Option<Motion>,
    ) -> Result<ActionResult, ControllerError> {
        let mut inner = self.inner.lock().map_err(|_| ControllerError::Poisoned)?;
        inner.target_last = None;
        inner.navigation_last = None;
        let result = self.act_inner(&mut inner, owner, lease_id, request, capture, motion);
        if let Ok(result) = &result {
            renew_verified_action(&mut inner.lease, owner, lease_id, result, Instant::now());
        }
        result
    }

    fn act_inner(
        &self,
        mut inner: &mut Inner<A>,
        owner: &str,
        lease_id: LeaseId,
        request: ActionRequest,
        capture: CaptureMode,
        motion: Option<Motion>,
    ) -> Result<ActionResult, ControllerError> {
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

        if let Some(result) = self.try_focused_text(&mut inner, &request, capture, motion)? {
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
        let lock_until = match (&request.action, admission) {
            (Action::Lock, Some(ActionAdmission::NativeLock { until }))
                if matches!(fresh.state.lock, LockState::Unlocked | LockState::Uncertain)
                    && fresh.state.wda_ready
                    && Instant::now() < until =>
            {
                Some(until)
            }
            _ => None,
        };
        if matches!(admission, Some(ActionAdmission::NativeLock { .. })) && lock_until.is_none() {
            return Err(ControllerError::StaleObservation);
        }
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
            && lock_until.is_none()
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
        if let Some(until) = lock_until {
            send_deadline = send_deadline.min(until);
        }
        let execution_admission =
            (admitted_home_launch || pixel_drift_until.is_some() || lock_until.is_some())
                .then_some(admission)
                .flatten();
        let receipt = match inner.adapter.execute_with_motion(
            &request.action,
            send_deadline,
            execution_admission,
            motion,
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
        inner.last = None;
        inner.target_last = None;
        inner.navigation_last = None;
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
        inner.target_last = None;
        inner.navigation_last = None;
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
