use crate::native_lock;
use crate::pin_field;
use crate::pin_screen::{self, ParsedPasscodeView};
use crate::usb::PinnedUsb;
use crate::wda_action::{WdaActions, WdaRequest};
use crate::wda_transport::{UsbWda, WdaError};
use handkite_core::controller::AdapterError;
use handkite_core::passcode::PasscodeProgress;
use handkite_core::protocol::{Action, HardwareId, LockState, Point, TransportReceipt};
use handkite_core::unlock::{BackendError, PasscodeBackend, PasscodeScreen};
use std::future::Future;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PIN_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const PIN_VIEW_BUDGET: Duration = Duration::from_secs(5);
const REVEAL_SETTLE_BUDGET: Duration = Duration::from_secs(10);
const REVEAL_INITIAL_SETTLE: Duration = Duration::from_millis(1500);
const REVEAL_SETTLE_PAUSE: Duration = Duration::from_millis(100);
const MAX_REVEAL_CAPTURES: usize = 3;
const OWNED_WAKE_SETTLE: Duration = Duration::from_millis(1000);
const SUPERVISED_WAKE_SETTLE: Duration = Duration::from_millis(1500);

mod owned_input;
use owned_input::InputRequests;

/// Fixed labels contain no credential, session or screen values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinPhase {
    InitialSnapshot,
    OwnedAdmission,
    WakeDispatch,
    WakePostcapture,
    WakeAdmission,
    NormalConfirmation,
    RevealDispatch,
    RevealPostcapture,
    EmptyConfirmation,
    DigitInput,
}

impl PinPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InitialSnapshot => "initial_snapshot",
            Self::OwnedAdmission => "owned_admission",
            Self::WakeDispatch => "wake_dispatch",
            Self::WakePostcapture => "wake_postcapture",
            Self::WakeAdmission => "wake_admission",
            Self::NormalConfirmation => "normal_confirmation",
            Self::RevealDispatch => "reveal_dispatch",
            Self::RevealPostcapture => "reveal_postcapture",
            Self::EmptyConfirmation => "empty_confirmation",
            Self::DigitInput => "digit_input",
        }
    }
}

/// Fixed capture checkpoints; never includes source XML, session IDs or field values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinCaptureStep {
    SessionBefore,
    SourceRead,
    LockRead,
    DirectField,
    SessionAfter,
    ParseSource,
    NativeLock,
    ViewportRead,
    Freshness,
    EmptyAdmission,
    RevealSettleLimit,
    Complete,
}

impl PinCaptureStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionBefore => "session_before",
            Self::SourceRead => "source_read",
            Self::LockRead => "lock_read",
            Self::DirectField => "direct_field",
            Self::SessionAfter => "session_after",
            Self::ParseSource => "parse_source",
            Self::NativeLock => "native_lock",
            Self::ViewportRead => "viewport_read",
            Self::Freshness => "freshness",
            Self::EmptyAdmission => "empty_admission",
            Self::RevealSettleLimit => "reveal_settle_limit",
            Self::Complete => "complete",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinCaptureTimings {
    pub session_before_ms: u128,
    pub source_ms: u128,
    pub lock_ms: u128,
    pub direct_field_ms: u128,
    pub session_after_ms: u128,
    pub parse_ms: u128,
    pub native_ms: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PinReportedLock {
    Locked,
    Unlocked,
    #[default]
    Unknown,
}

impl PinReportedLock {
    fn from_bool(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::Locked,
            Some(false) => Self::Unlocked,
            None => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "true",
            Self::Unlocked => "false",
            Self::Unknown => "unknown",
        }
    }
}

pub struct PinPreparationDiagnostic {
    pub outcome: &'static str,
    pub phase: PinPhase,
    pub capture_step: PinCaptureStep,
    pub elapsed_ms: u128,
    pub captures: usize,
    pub normal_locked: bool,
    pub empty_locked: bool,
    pub snapshot: Option<PinDiagnostics>,
    pub timings: PinCaptureTimings,
    pub wda_reported_lock: PinReportedLock,
    pub refusal: &'static str,
}

// Await every read, including native query completion after a failed WDA read.
// The native command must not block this current-thread runtime's USB futures.
async fn capture_reads<S, L, N: Send + 'static>(
    source: impl Future<Output = S>,
    lock: impl Future<Output = L>,
    native: impl FnOnce() -> N + Send + 'static,
) -> (S, L, Result<N, tokio::task::JoinError>) {
    let native = tokio::task::spawn_blocking(native);
    tokio::join!(source, lock, native)
}

struct PreparedPinView {
    view: ParsedPasscodeView,
    fresh_until: Instant,
    empty_locked: bool,
    normal_locked: bool,
    captured_at: Instant,
}

// This permits one reveal, never a digit or a replacement PreparedPinView.
struct NormalReveal {
    normal: pin_screen::NormalLockScreen,
    captured_at: Instant,
    fresh_until: Instant,
    session: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ProgressCheck {
    Disabled,
    BeforeFirst,
    Pending,
    Complete,
    Failed,
}

pub struct NativePasscodeBackend {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
    last_view: Option<PreparedPinView>,
    last_diagnostics: Option<PinDiagnostics>,
    phase: PinPhase,
    capture_step: PinCaptureStep,
    capture_timings: PinCaptureTimings,
    wda_reported_lock: PinReportedLock,
    supervised_visibility_fallback: bool,
    session_bound: bool,
    awaiting_first_digit: bool,
    wake_attempted: bool,
    reveal_attempted: bool,
    progress_check: ProgressCheck,
    coordinate_pin_taps: bool,
    input_admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    capture_count: usize,
    preparation_refusal: &'static str,
}

#[derive(Clone, Copy, Default)]
pub struct PinDiagnostics {
    pub snapshot_ms: u128,
    pub screen_size: bool,
    pub field_seen: bool,
    pub field_accepted: bool,
    pub keys_seen: usize,
    pub keys_eligible: usize,
    pub keys_visible: usize,
    pub keys_enabled: usize,
    pub field_visible: bool,
    pub field_enabled: bool,
    pub layout_valid: bool,
    pub visibility_fallback: bool,
    pub native_locked: bool,
    pub wda_locked: bool,
    pub visible_locked: bool,
    pub prior_unlock: bool,
    pub alert: bool,
    pub normal_marker: bool,
}

impl NativePasscodeBackend {
    pub fn new(expected: HardwareId) -> Result<Self, BackendError> {
        Self::new_with_supervised_visibility_fallback(expected, false)
    }

    pub fn new_with_supervised_visibility_fallback(
        expected: HardwareId,
        supervised_visibility_fallback: bool,
    ) -> Result<Self, BackendError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| BackendError::Unavailable)?;
        Ok(Self {
            usb: PinnedUsb::new(expected.clone()),
            expected,
            runtime,
            actions: WdaActions::default(),
            last_view: None,
            last_diagnostics: None,
            phase: PinPhase::InitialSnapshot,
            capture_step: PinCaptureStep::SessionBefore,
            capture_timings: PinCaptureTimings::default(),
            wda_reported_lock: PinReportedLock::Unknown,
            supervised_visibility_fallback,
            session_bound: false,
            awaiting_first_digit: false,
            wake_attempted: false,
            reveal_attempted: false,
            progress_check: ProgressCheck::Disabled,
            coordinate_pin_taps: false,
            input_admission: None,
            capture_count: 0,
            preparation_refusal: "none",
        })
    }

    /// Opt-in backend for a settings transaction; every input revalidates its session.
    pub fn new_for_session(
        expected: HardwareId,
        supervised_visibility_fallback: bool,
        session: &str,
    ) -> Result<Self, BackendError> {
        let actions =
            WdaActions::bound_to_existing(session).map_err(|_| BackendError::Unavailable)?;
        let mut backend = Self::new_with_supervised_visibility_fallback(
            expected,
            supervised_visibility_fallback,
        )?;
        backend.actions = actions;
        backend.session_bound = true;
        backend.progress_check = ProgressCheck::BeforeFirst;
        Ok(backend)
    }

    /// Consumes the backend without calling any input transport. Runs the same
    /// preparation checks as unlock, stopping before a normal-lock reveal.
    pub fn diagnose_preparation(self) -> PinPreparationDiagnostic {
        self.diagnose_preparation_with(Self::snapshot)
    }

    /// Runs owned wake/reveal preparation and the final empty check, never PIN input.
    pub fn prepare_keypad_only(self) -> PinPreparationDiagnostic {
        self.prepare_keypad_only_with(Self::snapshot, Self::show_keypad)
    }

    fn prepare_keypad_only_with(
        mut self,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
        prepare: impl FnOnce(&mut Self) -> Result<(), BackendError>,
    ) -> PinPreparationDiagnostic {
        let started = Instant::now();
        let result = (|| {
            snapshot(&mut self)?;
            prepare(&mut self)?;
            snapshot(&mut self)?;
            if !self
                .last_view
                .as_ref()
                .is_some_and(|view| view.empty_locked)
            {
                return Err(BackendError::Unavailable);
            }
            Ok(())
        })();
        PinPreparationDiagnostic {
            outcome: if result.is_ok() {
                "empty_confirmed"
            } else {
                "backend_unavailable"
            },
            phase: self.phase,
            capture_step: self.capture_step,
            elapsed_ms: started.elapsed().as_millis(),
            captures: self.capture_count,
            normal_locked: self
                .last_view
                .as_ref()
                .is_some_and(|view| view.normal_locked),
            empty_locked: self
                .last_view
                .as_ref()
                .is_some_and(|view| view.empty_locked),
            snapshot: self.last_diagnostics,
            timings: self.capture_timings,
            wda_reported_lock: self.wda_reported_lock,
            refusal: self.preparation_refusal,
        }
    }

    fn diagnose_preparation_with(
        mut self,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
    ) -> PinPreparationDiagnostic {
        let started = Instant::now();
        let mut captures = 0;
        let mut capture = |backend: &mut Self| {
            captures += 1;
            snapshot(backend)
        };
        let mut reveal_required = false;
        let result = (|| {
            capture(&mut self)?;
            self.prepare_owned_keypad_with(Instant::now(), &mut capture, |_, _| {
                reveal_required = true;
                Err(BackendError::Unavailable)
            })?;
            // Match the core's fresh empty check without marking a PIN attempt.
            capture(&mut self)?;
            Ok::<(), BackendError>(())
        })();
        PinPreparationDiagnostic {
            outcome: if reveal_required {
                "reveal_required"
            } else if result.is_ok() {
                "empty_confirmed"
            } else {
                "backend_unavailable"
            },
            phase: self.phase,
            capture_step: self.capture_step,
            elapsed_ms: started.elapsed().as_millis(),
            captures,
            normal_locked: reveal_required
                || self
                    .last_view
                    .as_ref()
                    .is_some_and(|view| view.normal_locked),
            empty_locked: self
                .last_view
                .as_ref()
                .is_some_and(|view| view.empty_locked),
            snapshot: self.last_diagnostics,
            timings: self.capture_timings,
            wda_reported_lock: self.wda_reported_lock,
            refusal: self.preparation_refusal,
        }
    }

    pub fn phase(&self) -> PinPhase {
        self.phase
    }

    pub fn capture_step(&self) -> PinCaptureStep {
        self.capture_step
    }

    pub fn capture_timings(&self) -> PinCaptureTimings {
        self.capture_timings
    }

    pub fn diagnostics(&self) -> Option<PinDiagnostics> {
        self.last_diagnostics
    }

    /// Recheck runner and service ownership at every session-bound input dispatch.
    /// Do not include the PIN stop gate: this attempt marks it before digit input.
    pub fn with_input_admission(
        mut self,
        admit: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, BackendError> {
        if !self.session_bound {
            return Err(BackendError::Unavailable);
        }
        self.input_admission = Some(Arc::new(admit));
        Ok(self)
    }

    fn admit_input(&self) -> Result<(), BackendError> {
        if self.input_admission.as_ref().is_some_and(|admit| !admit()) {
            Err(BackendError::Unavailable)
        } else {
            Ok(())
        }
    }

    /// Coordinate PIN taps are available only inside the supervised session bracket.
    pub fn with_coordinate_pin_taps(mut self) -> Result<Self, BackendError> {
        if !self.session_bound || !self.supervised_visibility_fallback {
            return Err(BackendError::Unavailable);
        }
        self.coordinate_pin_taps = true;
        Ok(self)
    }

    fn request_action(&mut self, action: &Action) -> Result<(), BackendError> {
        self.request_action_mode(action, self.coordinate_pin_taps)
    }

    fn request_action_mode(
        &mut self,
        action: &Action,
        coordinate: bool,
    ) -> Result<(), BackendError> {
        self.request_action_with(action, Instant::now(), |backend, action, fresh_until| {
            backend.dispatch_action_before(action, coordinate, fresh_until)
        })
    }

    fn dispatch_action_before(
        &mut self,
        action: &Action,
        coordinate: bool,
        fresh_until: Instant,
    ) -> Result<(), BackendError> {
        let wda = UsbWda::new(self.usb.clone())
            .with_write_deadline(fresh_until)
            .with_write_admission(self.input_admission.clone());
        self.runtime
            .block_on(async {
                if self.session_bound {
                    let wda = InputRequests {
                        wda: &wda,
                        admission: self.input_admission.as_deref(),
                    };
                    bound_action(&mut self.actions, &wda, action, coordinate).await
                } else {
                    self.actions.execute_existing(&wda, action).await
                }
            })
            .map(|_| ())
            .map_err(|_| BackendError::Unavailable)
    }

    fn request_action_with(
        &mut self,
        action: &Action,
        now: Instant,
        dispatch: impl FnOnce(&mut Self, &Action, Instant) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        let prepared = self.last_view.take().ok_or(BackendError::Unavailable)?;
        if now >= prepared.fresh_until
            || matches!(
                self.progress_check,
                ProgressCheck::Pending | ProgressCheck::Failed
            )
        {
            return Err(BackendError::Unavailable);
        }
        self.admit_input()?;
        dispatch(self, action, prepared.fresh_until)
    }

    fn press_digit_with(
        &mut self,
        digit: u8,
        dispatch: impl FnOnce(&mut Self, &Action) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        if matches!(
            self.progress_check,
            ProgressCheck::Pending | ProgressCheck::Failed
        ) {
            return Err(BackendError::Unavailable);
        }
        let at = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.keypad.as_ref())
            .and_then(|keys| keys.get(usize::from(digit)))
            .copied()
            .ok_or(BackendError::Unavailable)?;
        self.phase = PinPhase::DigitInput;
        self.awaiting_first_digit = false;
        let result = dispatch(self, &Action::Tap { at });
        if self.progress_check == ProgressCheck::BeforeFirst {
            self.progress_check = ProgressCheck::Pending;
        }
        result
    }

    fn prepare_owned_normal_with(
        &mut self,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
        mut input: impl FnMut(&mut Self, &Action, Instant) -> Result<(), BackendError>,
        check: impl FnOnce(&mut Self, &NormalReveal) -> Result<(), BackendError>,
        now: impl Fn() -> Instant,
        mut pause: impl FnMut(Duration),
    ) -> Result<(), BackendError> {
        self.phase = PinPhase::OwnedAdmission;
        if !self.session_bound
            || !self.supervised_visibility_fallback
            || self.input_admission.is_none()
            || self.wake_attempted
            || self.reveal_attempted
        {
            return Err(BackendError::Unavailable);
        }
        let result = (|| {
            self.admit_input()?;
            let first = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
            if now() >= first.fresh_until || !first.normal_locked {
                return Err(BackendError::Unavailable);
            }
            let normal = first.view.normal_lock.ok_or(BackendError::Unavailable)?;
            let first_at = first.captured_at;
            self.phase = PinPhase::NormalConfirmation;
            snapshot(self)?;
            self.admit_input()?;
            let confirmed = self.last_view.take().ok_or(BackendError::Unavailable)?;
            if now() >= confirmed.fresh_until
                || !confirmed.normal_locked
                || confirmed.captured_at <= first_at
                || confirmed.view.normal_lock != Some(normal)
            {
                return Err(BackendError::Unavailable);
            }
            let reveal = NormalReveal {
                normal,
                captured_at: confirmed.captured_at,
                fresh_until: confirmed.fresh_until,
                session: self
                    .actions
                    .prepared_session()
                    .ok_or(BackendError::Unavailable)?
                    .into(),
            };
            // Confirm the full UI before Home. Between Home and swipe, only
            // session/lock/viewport reads run; a full source read delays reveal.
            // Post-Home alert/nonempty checks move to the full post-reveal
            // capture, which must still prove strict empty before any digit.
            self.wake_attempted = true;
            self.phase = PinPhase::WakeDispatch;
            input(self, &Action::Home, reveal.fresh_until)?;
            pause(OWNED_WAKE_SETTLE);
            self.phase = PinPhase::WakeAdmission;
            self.admit_input()?;
            if now() >= reveal.fresh_until {
                self.capture_step = PinCaptureStep::Freshness;
                return Err(BackendError::Unavailable);
            }
            check(self, &reveal)?;
            self.admit_input()?;
            if !self.actions.prepared_session_matches(&reveal.session) {
                return Err(BackendError::Unavailable);
            }
            if now() >= reveal.fresh_until {
                self.capture_step = PinCaptureStep::Freshness;
                return Err(BackendError::Unavailable);
            }
            self.reveal_attempted = true;
            self.phase = PinPhase::RevealDispatch;
            let mut swipe = keypad_swipe(reveal.normal.size)?;
            if let Action::Swipe { duration_ms, .. } = &mut swipe {
                *duration_ms = 250;
            }
            input(self, &swipe, reveal.fresh_until)?;
            self.phase = PinPhase::RevealPostcapture;
            self.settle_owned_reveal_with(
                reveal.normal,
                reveal.captured_at,
                &mut snapshot,
                now,
                &mut pause,
            )
        })();
        if result.is_err() {
            self.last_view = None;
            self.progress_check = ProgressCheck::Failed;
        }
        result
    }

    fn check_normal_reveal(&mut self, reveal: &NormalReveal) -> Result<(), BackendError> {
        if !self.actions.prepared_session_matches(&reveal.session) {
            return Err(BackendError::Unavailable);
        }
        let wda = UsbWda::new(self.usb.clone())
            .with_write_deadline(reveal.fresh_until)
            .with_write_admission(self.input_admission.clone());
        self.capture_step = PinCaptureStep::SessionBefore;
        self.runtime
            .block_on(self.actions.prepare_existing(&wda))
            .map_err(|_| BackendError::Unavailable)?;
        let orientation_path = format!("/session/{}/orientation", reveal.session);
        let expected = self.expected.clone();
        if let Err(step) = self.runtime.block_on(read_normal_reveal(
            &wda,
            &orientation_path,
            reveal.normal.size,
            move || native_lock::query(&expected),
        )) {
            self.capture_step = step;
            return Err(BackendError::Unavailable);
        }
        self.capture_step = PinCaptureStep::SessionAfter;
        self.runtime
            .block_on(self.actions.prepare_existing(&wda))
            .map_err(|_| BackendError::Unavailable)?;
        if !self.actions.prepared_session_matches(&reveal.session) {
            return Err(BackendError::Unavailable);
        }
        Ok(())
    }

    fn prepare_owned_keypad_after_wake_with(
        &mut self,
        now: Instant,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
        mut input: impl FnMut(&mut Self, &Action) -> Result<(), BackendError>,
        settle: impl FnOnce(Duration),
    ) -> Result<(), BackendError> {
        self.phase = PinPhase::OwnedAdmission;
        if !self.session_bound
            || !self.supervised_visibility_fallback
            || self.input_admission.is_none()
            || self.wake_attempted
            || self.reveal_attempted
        {
            return Err(BackendError::Unavailable);
        }
        self.admit_input()?;
        let prepared = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
        if now >= prepared.fresh_until || !(prepared.normal_locked || prepared.empty_locked) {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        let before_wake = prepared.captured_at;
        // An uncertain Home acknowledgement must never permit a second wake.
        self.wake_attempted = true;
        self.phase = PinPhase::WakeDispatch;
        let result = (|| {
            input(self, &Action::Home)?;
            self.last_view = None;
            settle(OWNED_WAKE_SETTLE);
            self.phase = PinPhase::WakePostcapture;
            self.admit_input()?;
            snapshot(self)?;
            self.admit_input()?;
            let prepared = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
            if Instant::now() >= prepared.fresh_until {
                self.capture_step = PinCaptureStep::Freshness;
                return Err(BackendError::Unavailable);
            }
            if prepared.captured_at <= before_wake
                || !(prepared.normal_locked || prepared.empty_locked)
            {
                self.capture_step = PinCaptureStep::EmptyAdmission;
                return Err(BackendError::Unavailable);
            }
            self.prepare_owned_keypad_with(Instant::now(), &mut snapshot, |backend, action| {
                input(backend, action)
            })
        })();
        if result.is_err() {
            self.last_view = None;
            self.progress_check = ProgressCheck::Failed;
        }
        result
    }

    fn prepare_owned_keypad_with(
        &mut self,
        now: Instant,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
        swipe: impl FnOnce(&mut Self, &Action) -> Result<(), BackendError>,
    ) -> Result<(), BackendError> {
        self.phase = PinPhase::OwnedAdmission;
        if !self.session_bound
            || !self.supervised_visibility_fallback
            || self.input_admission.is_none()
            || self.reveal_attempted
        {
            return Err(BackendError::Unavailable);
        }
        self.admit_input()?;
        let prepared = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
        if now >= prepared.fresh_until {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        if prepared.empty_locked {
            self.awaiting_first_digit = true;
            self.phase = PinPhase::EmptyConfirmation;
            return Ok(());
        }
        if !prepared.normal_locked {
            return Err(BackendError::Unavailable);
        }
        let first = prepared.view.normal_lock.ok_or(BackendError::Unavailable)?;
        let first_at = prepared.captured_at;
        self.phase = PinPhase::NormalConfirmation;
        snapshot(self)?;
        let confirmed = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
        if !confirmed.normal_locked
            || confirmed.captured_at <= first_at
            || confirmed.view.normal_lock != Some(first)
        {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        let confirmed_at = confirmed.captured_at;
        // Consume this preparation even if the write acknowledgement is lost.
        self.reveal_attempted = true;
        self.phase = PinPhase::RevealDispatch;
        let result = (|| {
            swipe(self, &keypad_swipe(first.size)?)?;
            self.phase = PinPhase::RevealPostcapture;
            self.settle_owned_reveal_with(
                first,
                confirmed_at,
                &mut snapshot,
                Instant::now,
                thread::sleep,
            )
        })();
        if result.is_err() {
            self.last_view = None;
            self.progress_check = ProgressCheck::Failed;
        }
        result
    }

    fn settle_owned_reveal_with(
        &mut self,
        normal: pin_screen::NormalLockScreen,
        mut previous_at: Instant,
        mut snapshot: impl FnMut(&mut Self) -> Result<PasscodeScreen, BackendError>,
        now: impl Fn() -> Instant,
        mut pause: impl FnMut(Duration),
    ) -> Result<(), BackendError> {
        let deadline = now()
            .checked_add(REVEAL_SETTLE_BUDGET)
            .ok_or(BackendError::Unavailable)?;
        pause(REVEAL_INITIAL_SETTLE);
        for attempt in 0..MAX_REVEAL_CAPTURES {
            self.admit_input()?;
            // Reserve the unchanged complete capture budget; never extend a
            // capture's freshness to wait for the transition.
            if now()
                .checked_add(PIN_VIEW_BUDGET)
                .is_none_or(|end| end > deadline)
            {
                self.capture_step = PinCaptureStep::RevealSettleLimit;
                return Err(BackendError::Unavailable);
            }
            snapshot(self)?;
            self.admit_input()?;
            let observed_at = now();
            let view = self.last_view.as_ref().ok_or(BackendError::Unavailable)?;
            if observed_at >= deadline {
                self.capture_step = PinCaptureStep::RevealSettleLimit;
                return Err(BackendError::Unavailable);
            }
            if observed_at >= view.fresh_until {
                self.capture_step = PinCaptureStep::Freshness;
                return Err(BackendError::Unavailable);
            }
            if view.captured_at <= previous_at {
                return Err(BackendError::Unavailable);
            }
            previous_at = view.captured_at;
            if view.empty_locked {
                self.awaiting_first_digit = true;
                self.phase = PinPhase::EmptyConfirmation;
                return Ok(());
            }
            // The lock marker may change visibility during the reveal. Only
            // strict normal lock with unchanged geometry may keep settling.
            if !view.normal_locked
                || view
                    .view
                    .normal_lock
                    .is_none_or(|after| after.size != normal.size)
            {
                self.preparation_refusal = self.reveal_refusal(normal);
                self.capture_step = PinCaptureStep::EmptyAdmission;
                return Err(BackendError::Unavailable);
            }
            if attempt + 1 < MAX_REVEAL_CAPTURES {
                pause(REVEAL_SETTLE_PAUSE);
            }
        }
        self.capture_step = PinCaptureStep::RevealSettleLimit;
        Err(BackendError::Unavailable)
    }

    fn reveal_refusal(&self, normal: pin_screen::NormalLockScreen) -> &'static str {
        let Some(snapshot) = self.last_diagnostics else {
            return "unclassified";
        };
        if !snapshot.native_locked || !snapshot.wda_locked || !snapshot.visible_locked {
            return "lock_agreement";
        }
        if !snapshot.prior_unlock {
            return "prior_unlock";
        }
        if snapshot.alert {
            return "alert";
        }
        let Some(view) = &self.last_view else {
            return "unclassified";
        };
        if let Some(after) = view.view.normal_lock {
            if after.size != normal.size {
                return "normal_dimensions_changed";
            }
            if after.proud_visible != normal.proud_visible {
                return "normal_visibility_changed";
            }
        }
        if view
            .view
            .secure_field_value
            .as_deref()
            .and_then(|value| PasscodeProgress::parse(value, 6).ok())
            .is_some_and(|progress| progress.entered > 0)
        {
            return "nonempty";
        }
        if snapshot.field_seen || snapshot.keys_seen > 0 {
            return "keypad_incomplete";
        }
        "normal_unproven"
    }

    fn wake_hidden_with(
        &mut self,
        now: Instant,
        wake: impl FnOnce(&mut Self, Instant) -> Result<(), BackendError>,
        settle: impl FnOnce(Duration),
        snapshot: impl FnOnce(&mut Self) -> Result<PasscodeScreen, BackendError>,
    ) -> Result<(), BackendError> {
        let prepared = self.last_view.take().ok_or(BackendError::Unavailable)?;
        if !self.supervised_visibility_fallback
            || !self.session_bound
            || now >= prepared.fresh_until
            || !prepared.empty_locked
            || !prepared.view.visibility_fallback
        {
            return Err(BackendError::Unavailable);
        }
        self.admit_input()?;
        wake(self, prepared.fresh_until)?;
        settle(SUPERVISED_WAKE_SETTLE);
        // The core samples again before marking the gate. Keep the same zero-progress
        // requirement through that snapshot, until the first digit dispatch begins.
        self.awaiting_first_digit = true;
        snapshot(self)?;
        if !self
            .last_view
            .as_ref()
            .is_some_and(|view| view.empty_locked)
        {
            self.last_view = None;
            return Err(BackendError::Unavailable);
        }
        Ok(())
    }
}

async fn bound_action(
    actions: &mut WdaActions,
    wda: &impl WdaRequest,
    action: &Action,
    coordinate_pin_taps: bool,
) -> Result<TransportReceipt, AdapterError> {
    if coordinate_pin_taps {
        let Action::Tap { at } = action else {
            return Err(AdapterError);
        };
        actions.tap_with_existing_session(wda, *at).await
    } else {
        actions.execute_with_existing_session(wda, action).await
    }
}

fn empty_locked(screen: &PasscodeScreen, view: &ParsedPasscodeView, expected: &HardwareId) -> bool {
    screen.connected_usb == *expected
        && screen.wda_target == *expected
        && screen.native_lock == LockState::Locked
        && screen.visible_lock == LockState::Locked
        && screen.wda_lock == LockState::Locked
        && !screen.before_first_unlock
        && !screen.trust_prompt
        && screen.keypad_ready
        && !view.has_alert
        && view.visible_lock == LockState::Locked
        && view.layout_valid
        && view.keypad.is_some()
        && [&screen.secure_field_value, &view.secure_field_value]
            .into_iter()
            .all(|field| {
                field
                    .as_deref()
                    .and_then(|value| PasscodeProgress::parse(value, 6).ok())
                    .is_some_and(|progress| progress.entered == 0)
            })
}

fn compare_progress(
    screen: &PasscodeScreen,
    view: &ParsedPasscodeView,
    direct: u32,
) -> Result<(), BackendError> {
    if screen.native_lock != LockState::Locked
        || screen.visible_lock != LockState::Locked
        || screen.wda_lock != LockState::Locked
        || screen.before_first_unlock
        || screen.trust_prompt
        || view.has_alert
        || !screen.keypad_ready
        || !view.layout_valid
        || view.keypad.is_none()
    {
        return Err(BackendError::Unavailable);
    }
    let source = pin_field::count(
        view.secure_field_value
            .as_deref()
            .ok_or(BackendError::Unavailable)?,
    )
    .map_err(|_| BackendError::Unavailable)?;
    if source != direct {
        return Err(BackendError::ProgressMismatch);
    }
    Ok(())
}

fn wda_error(error: WdaError) -> BackendError {
    match error {
        WdaError::Usb(_) => BackendError::Disconnected,
        _ => BackendError::Unavailable,
    }
}

fn response_value(
    response: crate::wda_http::HttpResponse,
) -> Result<serde_json::Value, BackendError> {
    if response.status != 200 {
        return Err(BackendError::Unavailable);
    }
    let mut json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| BackendError::Unavailable)?;
    json.get_mut("value")
        .map(serde_json::Value::take)
        .ok_or(BackendError::Unavailable)
}

fn keypad_swipe(size: (f64, f64)) -> Result<Action, BackendError> {
    let (width, height) = size;
    if !width.is_finite()
        || !height.is_finite()
        || !(200.0..=2000.0).contains(&width)
        || !(400.0..=2000.0).contains(&height)
    {
        return Err(BackendError::Unavailable);
    }
    Ok(Action::Swipe {
        from: Point {
            x: width / 2.0,
            y: height - 8.0,
        },
        to: Point {
            x: width / 2.0,
            y: height * 0.45,
        },
        duration_ms: 300,
    })
}

fn normal_reveal_viewport_matches(
    screen: &serde_json::Value,
    orientation: &serde_json::Value,
    expected: (f64, f64),
) -> bool {
    let width = screen["screenSize"]["width"].as_f64();
    let height = screen["screenSize"]["height"].as_f64();
    orientation.as_str() == Some("PORTRAIT")
        && matches!((width, height), (Some(w), Some(h))
            if w.is_finite() && h.is_finite() && w > 0.0 && h > w
                && (w, h) == expected)
}

async fn read_normal_reveal(
    wda: &impl WdaRequest,
    orientation_path: &str,
    expected_size: (f64, f64),
    native: impl FnOnce() -> native_lock::NativeLock + Send + 'static,
) -> Result<(), PinCaptureStep> {
    let (viewport, locked, native) = capture_reads(
        async {
            tokio::join!(
                wda.request("GET", "/wda/screen", b"", PIN_VIEW_BUDGET),
                wda.request("GET", orientation_path, b"", PIN_VIEW_BUDGET)
            )
        },
        wda.request("GET", "/wda/locked", b"", PIN_VIEW_BUDGET),
        native,
    )
    .await;
    let value = |result: Result<_, _>, step| {
        result
            .map_err(wda_error)
            .and_then(response_value)
            .map_err(|_| step)
    };
    let screen = value(viewport.0, PinCaptureStep::ViewportRead)?;
    let orientation = value(viewport.1, PinCaptureStep::ViewportRead)?;
    if !normal_reveal_viewport_matches(&screen, &orientation, expected_size) {
        return Err(PinCaptureStep::ViewportRead);
    }
    if value(locked, PinCaptureStep::LockRead)?.as_bool() != Some(true) {
        return Err(PinCaptureStep::LockRead);
    }
    let native = native.map_err(|_| PinCaptureStep::NativeLock)?;
    if native.state != LockState::Locked || native.unlocked_since_boot != Some(true) {
        return Err(PinCaptureStep::NativeLock);
    }
    Ok(())
}

impl NativePasscodeBackend {
    fn snapshot_with(
        &mut self,
        started: Instant,
        capture: impl FnOnce(
            &mut Self,
            Instant,
        ) -> Result<(PasscodeScreen, ParsedPasscodeView), BackendError>,
        now: impl FnOnce() -> Instant,
    ) -> Result<PasscodeScreen, BackendError> {
        self.capture_count += 1;
        self.last_view = None;
        self.last_diagnostics = None;
        self.capture_timings = PinCaptureTimings::default();
        self.wda_reported_lock = PinReportedLock::Unknown;
        if self.progress_check == ProgressCheck::Failed {
            return Err(BackendError::Unavailable);
        }
        let comparing = self.progress_check == ProgressCheck::Pending;
        let fresh_until = started
            .checked_add(PIN_VIEW_BUDGET)
            .ok_or(BackendError::Unavailable)?;
        let captured = capture(self, started);
        if comparing {
            self.progress_check = ProgressCheck::Failed;
        }
        let (screen, view) = captured?;
        self.capture_step = PinCaptureStep::Freshness;
        if now() >= fresh_until {
            return Err(BackendError::Unavailable);
        }
        let empty_locked = empty_locked(&screen, &view, &self.expected);
        let normal_locked = screen.connected_usb == self.expected
            && screen.wda_target == self.expected
            && screen.native_lock == LockState::Locked
            && screen.wda_lock == LockState::Locked
            && screen.visible_lock == LockState::Locked
            && !screen.before_first_unlock
            && !screen.trust_prompt
            && !screen.keypad_ready
            && screen.secure_field_value.is_none()
            && view.normal_lock.is_some();
        if self.awaiting_first_digit && !empty_locked {
            self.capture_step = PinCaptureStep::EmptyAdmission;
            return Err(BackendError::Unavailable);
        }
        self.last_view = Some(PreparedPinView {
            view,
            fresh_until,
            empty_locked,
            normal_locked,
            captured_at: started,
        });
        if comparing {
            self.progress_check = ProgressCheck::Complete;
        }
        self.capture_step = PinCaptureStep::Complete;
        Ok(screen)
    }

    fn capture_snapshot(
        &mut self,
        started: Instant,
    ) -> Result<(PasscodeScreen, ParsedPasscodeView), BackendError> {
        let wda = UsbWda::new(self.usb.clone());
        self.capture_step = PinCaptureStep::SessionBefore;
        let step_started = Instant::now();
        let prepared = self.runtime.block_on(self.actions.prepare_existing(&wda));
        self.capture_timings.session_before_ms = step_started.elapsed().as_millis();
        prepared.map_err(|_| BackendError::Unavailable)?;
        let expected = self.expected.clone();
        let ((source, source_ms), (reported_lock, lock_ms), native) =
            self.runtime.block_on(capture_reads(
                async {
                    let started = Instant::now();
                    let result = wda
                        .request("GET", PIN_SOURCE, b"", Duration::from_secs(10))
                        .await;
                    (result, started.elapsed().as_millis())
                },
                async {
                    let started = Instant::now();
                    let result = wda
                        .request("GET", "/wda/locked", b"", Duration::from_secs(10))
                        .await;
                    (result, started.elapsed().as_millis())
                },
                move || {
                    let started = Instant::now();
                    let native = native_lock::query(&expected);
                    (native, started.elapsed().as_millis())
                },
            ));
        if let Ok((_, native_ms)) = &native {
            self.capture_timings.native_ms = *native_ms;
        }
        self.capture_timings.source_ms = source_ms;
        self.capture_timings.lock_ms = lock_ms;
        self.capture_step = PinCaptureStep::SourceRead;
        let source = response_value(source.map_err(wda_error)?)?;
        self.capture_step = PinCaptureStep::LockRead;
        let reported_lock = response_value(reported_lock.map_err(wda_error)?)?;
        self.wda_reported_lock = PinReportedLock::from_bool(reported_lock.as_bool());
        let direct = if self.progress_check == ProgressCheck::Pending {
            self.capture_step = PinCaptureStep::DirectField;
            let session = self
                .actions
                .prepared_session()
                .ok_or(BackendError::Unavailable)?;
            let step_started = Instant::now();
            let direct = self.runtime.block_on(pin_field::read(&wda, session));
            self.capture_timings.direct_field_ms = step_started.elapsed().as_millis();
            Some(direct.map_err(|_| BackendError::Unavailable)?)
        } else {
            None
        };
        if self.session_bound {
            self.capture_step = PinCaptureStep::SessionAfter;
            let step_started = Instant::now();
            let prepared = self.runtime.block_on(self.actions.prepare_existing(&wda));
            self.capture_timings.session_after_ms = step_started.elapsed().as_millis();
            prepared.map_err(|_| BackendError::Unavailable)?;
        }
        self.capture_step = PinCaptureStep::ParseSource;
        let xml = source.as_str().ok_or(BackendError::Unavailable)?;
        let step_started = Instant::now();
        let parsed = pin_screen::parse(xml);
        self.capture_timings.parse_ms = step_started.elapsed().as_millis();
        let parsed = parsed.map_err(|_| BackendError::Unavailable)?;
        self.capture_step = PinCaptureStep::NativeLock;
        let (native, _) = native.map_err(|_| BackendError::Unavailable)?;
        self.last_diagnostics = Some(PinDiagnostics {
            snapshot_ms: started.elapsed().as_millis(),
            screen_size: parsed.screen_size.is_some(),
            field_seen: parsed.field_seen,
            field_accepted: parsed.secure_field_value.is_some(),
            keys_seen: parsed.digit_keys_seen,
            keys_eligible: parsed.digit_keys_eligible,
            keys_visible: parsed.digit_keys_visible,
            keys_enabled: parsed.digit_keys_enabled,
            field_visible: parsed.field_visible,
            field_enabled: parsed.field_enabled,
            layout_valid: parsed.layout_valid,
            visibility_fallback: parsed.visibility_fallback,
            native_locked: native.state == LockState::Locked,
            wda_locked: reported_lock.as_bool() == Some(true),
            visible_locked: parsed.visible_lock == LockState::Locked,
            prior_unlock: native.unlocked_since_boot == Some(true),
            alert: parsed.has_alert,
            normal_marker: parsed.normal_lock.is_some(),
        });
        let wda_lock = match reported_lock.as_bool() {
            Some(true) => LockState::Locked,
            Some(false) => LockState::Unlocked,
            None => LockState::Uncertain,
        };
        let fallback_blocked = parsed.visibility_fallback && !self.supervised_visibility_fallback;
        let screen = PasscodeScreen {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            native_lock: native.state,
            visible_lock: parsed.visible_lock,
            wda_lock,
            keypad_ready: !fallback_blocked
                && parsed.keypad.is_some()
                && parsed.secure_field_value.is_some(),
            secure_field_value: (!fallback_blocked)
                .then(|| parsed.secure_field_value.clone())
                .flatten(),
            before_first_unlock: native.unlocked_since_boot != Some(true),
            trust_prompt: parsed.has_alert,
        };
        if let Some(direct) = direct {
            compare_progress(&screen, &parsed, direct)?;
        }
        Ok((screen, parsed))
    }
}

impl PasscodeBackend for NativePasscodeBackend {
    fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
        self.snapshot_with(Instant::now(), Self::capture_snapshot, Instant::now)
    }

    fn show_keypad(&mut self) -> Result<(), BackendError> {
        if matches!(
            self.progress_check,
            ProgressCheck::Pending | ProgressCheck::Failed
        ) {
            return Err(BackendError::Unavailable);
        }
        if self.supervised_visibility_fallback
            && self.session_bound
            && self.input_admission.is_some()
        {
            if self
                .last_view
                .as_ref()
                .is_some_and(|view| view.normal_locked)
            {
                return self.prepare_owned_normal_with(
                    Self::snapshot,
                    |backend, action, deadline| {
                        backend.dispatch_action_before(action, false, deadline)
                    },
                    Self::check_normal_reveal,
                    Instant::now,
                    thread::sleep,
                );
            }
            return self.prepare_owned_keypad_after_wake_with(
                Instant::now(),
                Self::snapshot,
                |backend, action| backend.request_action_mode(action, false),
                thread::sleep,
            );
        }
        if self.supervised_visibility_fallback && self.session_bound {
            if self.last_view.as_ref().is_some_and(|prepared| {
                prepared.empty_locked
                    && !prepared.view.visibility_fallback
                    && prepared.view.field_visible
                    && prepared.view.digit_keys_visible == 10
            }) {
                return Ok(());
            }
            return self.wake_hidden_with(
                Instant::now(),
                |backend, fresh_until| {
                    let wda = UsbWda::new(backend.usb.clone())
                        .with_write_deadline(fresh_until)
                        .with_write_admission(backend.input_admission.clone());
                    let wda = InputRequests {
                        wda: &wda,
                        admission: backend.input_admission.as_deref(),
                    };
                    backend
                        .runtime
                        .block_on(backend.actions.wake_with_existing_session(&wda))
                        .map_err(|_| BackendError::Unavailable)
                },
                thread::sleep,
                Self::snapshot,
            );
        }
        if self.supervised_visibility_fallback {
            return self
                .last_view
                .as_ref()
                .filter(|prepared| {
                    prepared.view.visibility_fallback && prepared.view.keypad.is_some()
                })
                .map(|_| ())
                .ok_or(BackendError::Unavailable);
        }
        if self
            .last_view
            .as_ref()
            .is_some_and(|prepared| prepared.view.keypad.is_some())
        {
            return Ok(());
        }
        let prepared = self.last_view.take();
        let wda = UsbWda::new(self.usb.clone());
        if self.session_bound {
            let deadline = prepared.ok_or(BackendError::Unavailable)?.fresh_until;
            let wda = wda
                .with_write_deadline(deadline)
                .with_write_admission(self.input_admission.clone());
            self.admit_input()?;
            let wda = InputRequests {
                wda: &wda,
                admission: self.input_admission.as_deref(),
            };
            self.runtime
                .block_on(self.actions.wake_with_existing_session(&wda))
                .map_err(|_| BackendError::Unavailable)?;
        } else {
            self.runtime
                .block_on(self.actions.wake_hid_existing(&wda))
                .map_err(|_| BackendError::Unavailable)?;
        }
        thread::sleep(Duration::from_millis(300));
        let screen = self.snapshot()?;
        if screen.keypad_ready {
            return Ok(());
        }
        let size = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.screen_size)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&keypad_swipe(size)?)?;
        for _ in 0..3 {
            thread::sleep(Duration::from_millis(150));
            if self.snapshot()?.keypad_ready {
                return Ok(());
            }
        }
        Err(BackendError::Unavailable)
    }

    fn press_digit(&mut self, digit: u8) -> Result<(), BackendError> {
        self.press_digit_with(digit, Self::request_action)
    }

    fn delete_digit(&mut self) -> Result<(), BackendError> {
        if self.supervised_visibility_fallback {
            return Err(BackendError::Unavailable);
        }
        let at = self
            .last_view
            .as_ref()
            .and_then(|prepared| prepared.view.delete_key)
            .ok_or(BackendError::Unavailable)?;
        self.request_action(&Action::Tap { at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> NativePasscodeBackend {
        NativePasscodeBackend::new(HardwareId::new("fixture-phone").unwrap()).unwrap()
    }

    fn capture() -> (PasscodeScreen, ParsedPasscodeView) {
        let id = HardwareId::new("fixture-phone").unwrap();
        (
            PasscodeScreen {
                connected_usb: id.clone(),
                wda_target: id,
                native_lock: LockState::Locked,
                visible_lock: LockState::Locked,
                wda_lock: LockState::Locked,
                keypad_ready: false,
                secure_field_value: None,
                before_first_unlock: false,
                trust_prompt: false,
            },
            pin_screen::parse("<XCUIElementTypeApplication width=\"400\" height=\"800\"/>")
                .unwrap(),
        )
    }

    fn prepare(backend: &mut NativePasscodeBackend, started: Instant) {
        backend
            .snapshot_with(started, |_, _| Ok(capture()), || started)
            .unwrap();
    }

    fn hidden_capture() -> (PasscodeScreen, ParsedPasscodeView) {
        keypad_capture(true)
    }

    fn keypad_capture(hidden: bool) -> (PasscodeScreen, ParsedPasscodeView) {
        let mut xml = String::from(
            r#"<XCUIElementTypeApplication width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..10 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="false" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + column * 100, 300 + row * 100));
        }
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="false" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/></XCUIElementTypeApplication>"#);
        let xml = if hidden {
            xml
        } else {
            xml.replace("visible=\"false\"", "visible=\"true\"")
        };
        let view = pin_screen::parse(&xml).unwrap();
        let (mut screen, _) = capture();
        screen.keypad_ready = true;
        screen.secure_field_value = view.secure_field_value.clone();
        (screen, view)
    }

    fn bound_backend() -> NativePasscodeBackend {
        NativePasscodeBackend::new_for_session(
            HardwareId::new("fixture-phone").unwrap(),
            true,
            "fixture-session",
        )
        .unwrap()
    }

    #[test]
    fn owned_input_admission_requires_a_bound_session_and_precedes_dispatch() {
        assert!(backend().with_input_admission(|| true).is_err());
        let mut backend = bound_backend().with_input_admission(|| false).unwrap();
        let now = Instant::now();
        prepare(&mut backend, now);
        assert_eq!(
            backend.request_action_with(
                &Action::Tap {
                    at: Point { x: 100.0, y: 200.0 },
                },
                now,
                |_, _, _| panic!("lost ownership must precede dispatch"),
            ),
            Err(BackendError::Unavailable)
        );
        backend
            .snapshot_with(now, |_, _| Ok(hidden_capture()), || now)
            .unwrap();
        assert_eq!(
            backend.wake_hidden_with(
                now,
                |_, _| panic!("lost ownership must precede wake"),
                |_| panic!("refused wake must not settle"),
                |_| panic!("refused wake must not sample"),
            ),
            Err(BackendError::Unavailable)
        );
    }

    fn progress_capture(entered: u32) -> (PasscodeScreen, ParsedPasscodeView) {
        let (mut screen, mut view) = keypad_capture(false);
        let value = format!("{entered} of 6 digits");
        screen.secure_field_value = Some(value.clone());
        view.secure_field_value = Some(value);
        (screen, view)
    }

    #[test]
    fn coordinate_selection_requires_a_supervised_bound_backend() {
        assert!(backend().with_coordinate_pin_taps().is_err());
        assert!(
            NativePasscodeBackend::new_with_supervised_visibility_fallback(
                HardwareId::new("fixture-phone").unwrap(),
                true
            )
            .unwrap()
            .with_coordinate_pin_taps()
            .is_err()
        );
        assert!(
            NativePasscodeBackend::new_for_session(
                HardwareId::new("fixture-phone").unwrap(),
                false,
                "fixture-session"
            )
            .unwrap()
            .with_coordinate_pin_taps()
            .is_err()
        );
        assert!(!bound_backend().coordinate_pin_taps);
        assert!(
            bound_backend()
                .with_coordinate_pin_taps()
                .unwrap()
                .coordinate_pin_taps
        );
    }

    #[test]
    fn all_six_digit_taps_keep_the_selected_route_geometry_and_first_comparison() {
        use crate::wda_http::HttpResponse;
        use serde_json::{Value, json};
        use std::cell::Cell;
        struct Fake {
            coordinate: bool,
            calls: Cell<usize>,
            at: Cell<Point>,
        }
        impl WdaRequest for Fake {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, WdaError> {
                let call = self.calls.get();
                self.calls.set(call + 1);
                let value = match call % 3 {
                    0 => {
                        assert_eq!((method, path), ("GET", "/status"));
                        assert!(body.is_empty());
                        json!({"sessionId":"fixture-session","value":{"ready":true}})
                    }
                    1 => {
                        assert_eq!((method, path), ("GET", "/session/fixture-session"));
                        assert!(body.is_empty());
                        json!({"value":{}})
                    }
                    _ => {
                        let endpoint = if self.coordinate {
                            "/session/fixture-session/wda/tap"
                        } else {
                            "/session/fixture-session/actions"
                        };
                        assert_eq!((method, path), ("POST", endpoint));
                        let body: Value = serde_json::from_slice(body).unwrap();
                        let at = self.at.get();
                        if self.coordinate {
                            assert_eq!(body, json!({"x":at.x,"y":at.y}));
                        } else {
                            let movement = &body["actions"][0]["actions"][0];
                            assert_eq!(movement["x"], at.x);
                            assert_eq!(movement["y"], at.y);
                        }
                        json!({"value":null})
                    }
                };
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap(),
                })
            }
        }
        for coordinate in [false, true] {
            let mut backend = bound_backend();
            if coordinate {
                backend = backend.with_coordinate_pin_taps().unwrap();
            }
            let fake = Fake {
                coordinate,
                calls: Cell::new(0),
                at: Cell::new(Point { x: 0.0, y: 0.0 }),
            };
            let start = Instant::now();
            let mut comparisons = 0;
            for entered in 0..6 {
                backend
                    .snapshot_with(
                        start,
                        |backend, _| {
                            let (screen, view) = progress_capture(entered);
                            if backend.progress_check == ProgressCheck::Pending {
                                comparisons += 1;
                                compare_progress(&screen, &view, entered)?;
                            }
                            Ok((screen, view))
                        },
                        || start,
                    )
                    .unwrap();
                let digit = entered as u8;
                fake.at.set(
                    backend.last_view.as_ref().unwrap().view.keypad.unwrap()[usize::from(digit)],
                );
                backend
                    .press_digit_with(digit, |backend, action| {
                        backend.request_action_with(action, start, |backend, action, expires| {
                            assert_eq!(expires, start + PIN_VIEW_BUDGET);
                            backend
                                .runtime
                                .block_on(bound_action(
                                    &mut backend.actions,
                                    &fake,
                                    action,
                                    backend.coordinate_pin_taps,
                                ))
                                .map(|_| ())
                                .map_err(|_| BackendError::Unavailable)
                        })
                    })
                    .unwrap();
                assert!(backend.last_view.is_none());
            }
            assert_eq!(fake.calls.get(), 18);
            assert_eq!(comparisons, 1);
            assert_eq!(backend.progress_check, ProgressCheck::Complete);
            assert_eq!(backend.delete_digit(), Err(BackendError::Unavailable));
            assert_eq!(fake.calls.get(), 18);
        }
    }

    #[test]
    fn first_digit_comparison_runs_once_only_for_the_bound_backend() {
        for bound in [false, true] {
            let mut backend = if bound { bound_backend() } else { backend() };
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(progress_capture(0)), || start)
                .unwrap();
            let mut reads = 0;
            for entered in 1..=2 {
                backend
                    .press_digit_with(0, |backend, action| {
                        backend.request_action_with(action, start, |_, _, _| Ok(()))
                    })
                    .unwrap();
                backend
                    .snapshot_with(
                        start,
                        |backend, _| {
                            let (screen, view) = progress_capture(entered);
                            if backend.progress_check == ProgressCheck::Pending {
                                reads += 1;
                                compare_progress(&screen, &view, entered)?;
                            }
                            Ok((screen, view))
                        },
                        || start,
                    )
                    .unwrap();
            }
            assert_eq!(reads, usize::from(bound));
            assert_eq!(
                backend.progress_check,
                if bound {
                    ProgressCheck::Complete
                } else {
                    ProgressCheck::Disabled
                }
            );
        }
    }

    #[test]
    fn failed_comparison_consumes_the_snapshot_and_blocks_every_later_input() {
        for direct in [Ok(1), Err(BackendError::Unavailable)] {
            let mut backend = bound_backend();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(progress_capture(0)), || start)
                .unwrap();
            backend
                .press_digit_with(0, |backend, action| {
                    backend.request_action_with(action, start, |_, _, _| Ok(()))
                })
                .unwrap();
            let mut reads = 0;
            let result = backend.snapshot_with(
                start,
                |_, _| {
                    reads += 1;
                    let (screen, view) = progress_capture(0);
                    compare_progress(&screen, &view, direct?)?;
                    Ok((screen, view))
                },
                || start,
            );
            assert_eq!(
                result.err(),
                Some(if direct.is_ok() {
                    BackendError::ProgressMismatch
                } else {
                    BackendError::Unavailable
                })
            );
            assert_eq!(reads, 1);
            assert!(backend.last_view.is_none());
            assert_eq!(backend.progress_check, ProgressCheck::Failed);
            assert_eq!(
                backend.press_digit_with(0, |_, _| panic!("no next digit")),
                Err(BackendError::Unavailable)
            );
            assert!(
                backend
                    .snapshot_with(start, |_, _| panic!("no repeat comparison"), || start)
                    .is_err()
            );
            assert_eq!(backend.show_keypad(), Err(BackendError::Unavailable));
        }
    }

    #[test]
    fn comparison_failure_keeps_the_marked_gate_and_stops_before_digit_two() {
        use handkite_core::passcode::Passcode;
        use handkite_core::protocol::{DeviceBinding, DeviceKey};
        use handkite_core::unlock::{
            AttemptGate, GateError, UnlockError, UnlockPolicy, unlock_once,
        };
        struct Harness {
            backend: NativePasscodeBackend,
            start: Instant,
            taps: usize,
            reads: usize,
        }
        impl PasscodeBackend for Harness {
            fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
                let reads = &mut self.reads;
                self.backend.snapshot_with(
                    self.start,
                    |backend, _| {
                        let (screen, view) = progress_capture(0);
                        if backend.progress_check == ProgressCheck::Pending {
                            *reads += 1;
                            compare_progress(&screen, &view, 1)?;
                        }
                        Ok((screen, view))
                    },
                    || self.start,
                )
            }
            fn show_keypad(&mut self) -> Result<(), BackendError> {
                self.backend.show_keypad()
            }
            fn press_digit(&mut self, digit: u8) -> Result<(), BackendError> {
                let taps = &mut self.taps;
                let start = self.start;
                self.backend.press_digit_with(digit, |backend, action| {
                    backend.request_action_with(action, start, |_, _, _| {
                        *taps += 1;
                        Ok(())
                    })
                })
            }
            fn delete_digit(&mut self) -> Result<(), BackendError> {
                panic!("no deletion")
            }
        }
        #[derive(Default)]
        struct Gate {
            marked: bool,
        }
        impl AttemptGate for Gate {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(self.marked)
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                self.marked = true;
                Ok(())
            }
            fn clear(&mut self) -> Result<(), GateError> {
                panic!("mismatch cannot clear")
            }
        }
        for coordinate in [false, true] {
            let mut harness = Harness {
                backend: if coordinate {
                    bound_backend().with_coordinate_pin_taps().unwrap()
                } else {
                    bound_backend()
                },
                start: Instant::now(),
                taps: 0,
                reads: 0,
            };
            let mut gate = Gate::default();
            let binding = DeviceBinding {
                key: DeviceKey::new("iphone").unwrap(),
                usb_id: HardwareId::new("fixture-phone").unwrap(),
            };
            let code = Passcode::new("0".repeat(6)).unwrap();
            assert_eq!(
                unlock_once(
                    &mut harness,
                    &mut gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                ),
                Err(UnlockError::Backend(BackendError::ProgressMismatch))
            );
            assert_eq!((harness.taps, harness.reads), (1, 1));
            assert!(gate.marked);
            assert_eq!(
                unlock_once(
                    &mut harness,
                    &mut gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                ),
                Err(UnlockError::Disabled)
            );
            assert_eq!((harness.taps, harness.reads), (1, 1));
        }
    }

    fn normal_capture() -> (PasscodeScreen, ParsedPasscodeView) {
        let (screen, _) = capture();
        let view = pin_screen::parse(r#"<AppiumAUT><XCUIElementTypeApplication bundleId="com.apple.springboard" x="0" y="0" width="400" height="800"><XCUIElementTypeOther name="proud-lock" visible="true"/></XCUIElementTypeApplication></AppiumAUT>"#).unwrap();
        (screen, view)
    }

    #[test]
    fn fast_normal_reveal_confirms_before_home_and_keeps_original_deadline() {
        use std::cell::{Cell, RefCell};
        let mut backend = bound_backend().with_input_admission(|| true).unwrap();
        let start = Instant::now();
        let clock = Cell::new(start);
        let events = RefCell::new(Vec::new());
        backend
            .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
            .unwrap();
        let confirmed_at = start + Duration::from_millis(1);
        let deadline = confirmed_at + PIN_VIEW_BUDGET;
        let mut captures = 0;
        backend
            .prepare_owned_normal_with(
                |backend| {
                    captures += 1;
                    events.borrow_mut().push("capture");
                    clock.set(clock.get() + Duration::from_millis(1));
                    let at = clock.get();
                    backend.snapshot_with(
                        at,
                        |_, _| {
                            Ok(if captures == 1 {
                                normal_capture()
                            } else {
                                keypad_capture(true)
                            })
                        },
                        || at,
                    )
                },
                |backend, action, fresh_until| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(fresh_until, deadline);
                    match action {
                        Action::Home => events.borrow_mut().push("home"),
                        Action::Swipe {
                            from,
                            to,
                            duration_ms,
                        } => {
                            assert_eq!(*from, Point { x: 200.0, y: 792.0 });
                            assert_eq!(*to, Point { x: 200.0, y: 360.0 });
                            assert_eq!(*duration_ms, 250);
                            events.borrow_mut().push("swipe");
                        }
                        _ => panic!("no PIN input during preparation"),
                    }
                    Ok(())
                },
                |backend, reveal| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(reveal.captured_at, confirmed_at);
                    assert_eq!(reveal.fresh_until, deadline);
                    events.borrow_mut().push("guard");
                    Ok(())
                },
                || clock.get(),
                |delay| {
                    events.borrow_mut().push(if delay == OWNED_WAKE_SETTLE {
                        "wake_wait"
                    } else {
                        assert_eq!(delay, REVEAL_INITIAL_SETTLE);
                        "reveal_wait"
                    });
                    clock.set(clock.get() + delay);
                },
            )
            .unwrap();
        assert_eq!(
            *events.borrow(),
            [
                "capture",
                "home",
                "wake_wait",
                "guard",
                "swipe",
                "reveal_wait",
                "capture"
            ]
        );
        assert!(backend.awaiting_first_digit);
        assert!(backend.last_view.as_ref().unwrap().empty_locked);
        assert!(
            backend
                .prepare_owned_normal_with(
                    |_| panic!("no recapture"),
                    |_, _, _| panic!("no repeated input"),
                    |_, _| panic!("no repeated guards"),
                    || clock.get(),
                    |_| panic!("no wait"),
                )
                .is_err()
        );
    }

    #[test]
    fn fast_normal_reveal_refuses_changed_confirmation_before_home() {
        for case in 0..9 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let result = backend.prepare_owned_normal_with(
                |backend| {
                    let (mut screen, mut view) = normal_capture();
                    match case {
                        0 => view.normal_lock.as_mut().unwrap().proud_visible = false,
                        1 => view.normal_lock.as_mut().unwrap().size.0 += 1.0,
                        2 => screen.native_lock = LockState::Unlocked,
                        3 => screen.wda_lock = LockState::Uncertain,
                        4 => {
                            screen.trust_prompt = true;
                            view.has_alert = true;
                        }
                        5 => screen.before_first_unlock = true,
                        6 => {
                            (screen, view) = keypad_capture(true);
                        }
                        7 => return Err(BackendError::Unavailable),
                        8 => {}
                        _ => unreachable!(),
                    }
                    let at = if case == 8 {
                        start
                    } else {
                        start + Duration::from_millis(1)
                    };
                    backend.snapshot_with(at, |_, _| Ok((screen, view)), || at)
                },
                |_, _, _| panic!("unconfirmed normal cannot wake"),
                |_, _| panic!("unconfirmed normal cannot reach guards"),
                || start,
                |_| panic!("no wait"),
            );
            assert!(result.is_err(), "case {case}");
            assert!(!backend.wake_attempted);
            assert!(!backend.reveal_attempted);
            assert!(backend.last_view.is_none());
        }
    }

    #[test]
    fn fast_normal_reveal_stops_on_failure_expiry_or_lost_ownership() {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicBool, Ordering};
        for case in 0..8 {
            let owned = Arc::new(AtomicBool::new(true));
            let admission = Arc::clone(&owned);
            let mut backend = bound_backend()
                .with_input_admission(move || admission.load(Ordering::SeqCst))
                .unwrap();
            let start = Instant::now();
            let clock = Cell::new(start);
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let mut inputs = 0;
            let result = backend.prepare_owned_normal_with(
                |backend| {
                    let at = start + Duration::from_millis(1);
                    clock.set(at);
                    backend.snapshot_with(at, |_, _| Ok(normal_capture()), || at)
                },
                |backend, action, deadline| {
                    assert!(backend.last_view.is_none());
                    assert!(clock.get() < deadline);
                    inputs += 1;
                    if matches!(action, Action::Home) && case == 0
                        || matches!(action, Action::Swipe { .. }) && case == 6
                    {
                        Err(BackendError::Unavailable)
                    } else {
                        Ok(())
                    }
                },
                |backend, _| {
                    if case == 3 {
                        return Err(BackendError::Unavailable);
                    }
                    if case == 4 {
                        owned.store(false, Ordering::SeqCst);
                    }
                    if case == 5 {
                        clock.set(start + Duration::from_secs(6));
                    }
                    if case == 7 {
                        backend.actions = WdaActions::bound_to_existing("changed-session").unwrap();
                    }
                    Ok(())
                },
                || clock.get(),
                |_| {
                    if case == 1 {
                        clock.set(start + Duration::from_secs(6));
                    }
                    if case == 2 {
                        owned.store(false, Ordering::SeqCst);
                    }
                },
            );
            assert!(result.is_err(), "case {case}");
            assert_eq!(inputs, if case == 6 { 2 } else { 1 }, "case {case}");
            assert!(backend.wake_attempted);
            assert_eq!(backend.reveal_attempted, case == 6);
            assert_eq!(backend.progress_check, ProgressCheck::Failed);
            assert!(backend.last_view.is_none());
            assert!(!backend.awaiting_first_digit);
            assert!(
                backend
                    .prepare_owned_normal_with(
                        |_| panic!("no retry"),
                        |_, _, _| panic!("no input retry"),
                        |_, _| panic!("no guard retry"),
                        || clock.get(),
                        |_| panic!("no wait"),
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn fast_normal_reveal_never_admits_digits_from_an_unsafe_postcapture() {
        use std::cell::Cell;
        for case in 0..3 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let clock = Cell::new(start);
            let mut captures = 0;
            let mut inputs = 0;
            let result = backend.prepare_owned_normal_with(
                |backend| {
                    captures += 1;
                    let at = clock.get() + Duration::from_millis(1);
                    clock.set(at);
                    let (mut screen, mut view) = if captures == 1 {
                        normal_capture()
                    } else {
                        keypad_capture(true)
                    };
                    if captures > 1 {
                        match case {
                            0 => {
                                screen.secure_field_value = Some("1 of 6 digits".into());
                                view.secure_field_value = screen.secure_field_value.clone();
                            }
                            1 => {
                                screen.trust_prompt = true;
                                view.has_alert = true;
                            }
                            2 => screen.native_lock = LockState::Unlocked,
                            _ => unreachable!(),
                        }
                    }
                    backend.snapshot_with(at, |_, _| Ok((screen, view)), || at)
                },
                |_, action, _| {
                    assert!(matches!(action, Action::Home | Action::Swipe { .. }));
                    inputs += 1;
                    Ok(())
                },
                |_, _| Ok(()),
                || clock.get(),
                |delay| clock.set(clock.get() + delay),
            );
            assert!(result.is_err(), "case {case}");
            assert_eq!(inputs, 2);
            assert_eq!(captures, 2);
            assert!(!backend.awaiting_first_digit);
            assert!(backend.last_view.is_none());
            assert!(
                backend
                    .press_digit_with(0, |_, _| panic!("no digit after failed preparation"))
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn fast_normal_reveal_reads_only_viewport_orientation_and_lock() {
        use crate::wda_http::HttpResponse;
        use serde_json::json;
        use std::cell::{Cell, RefCell};
        struct Reads {
            case: usize,
            paths: RefCell<Vec<String>>,
            started: Cell<u8>,
        }
        impl WdaRequest for Reads {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, WdaError> {
                assert_eq!(method, "GET");
                assert!(body.is_empty());
                self.paths.borrow_mut().push(path.into());
                let bit = match path {
                    "/wda/screen" => 1,
                    "/session/fixture-session/orientation" => 2,
                    _ => 0,
                };
                self.started.set(self.started.get() | bit);
                tokio::task::yield_now().await;
                if bit != 0 {
                    assert_eq!(self.started.get(), 3, "viewport reads must overlap");
                }
                if self.case == 9 && path == "/wda/screen" {
                    return Err(WdaError::NotConnected);
                }
                let value = match path {
                    "/wda/screen" => match self.case {
                        1 => json!({"screenSize":{"width":401,"height":800}}),
                        2 => json!({"screenSize":{"width":0,"height":800}}),
                        3 => json!({}),
                        _ => json!({"screenSize":{"width":400,"height":800}}),
                    },
                    "/session/fixture-session/orientation" => {
                        if self.case == 4 {
                            json!("LANDSCAPE")
                        } else {
                            json!("PORTRAIT")
                        }
                    }
                    "/wda/locked" => match self.case {
                        5 => json!(false),
                        6 => json!(null),
                        _ => json!(true),
                    },
                    _ => panic!("unexpected read"),
                };
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&json!({"value":value})).unwrap(),
                })
            }
        }
        for case in 0..12 {
            let reads = Reads {
                case,
                paths: RefCell::new(Vec::new()),
                started: Cell::new(0),
            };
            let result = read_normal_reveal(
                &reads,
                "/session/fixture-session/orientation",
                (400.0, 800.0),
                move || native_lock::NativeLock {
                    state: match case {
                        7 => LockState::Uncertain,
                        10 => LockState::Unlocked,
                        _ => LockState::Locked,
                    },
                    unlocked_since_boot: match case {
                        8 => None,
                        11 => Some(false),
                        _ => Some(true),
                    },
                },
            )
            .await;
            assert_eq!(result.is_ok(), case == 0, "case {case}");
            if case == 5 || case == 6 {
                assert_eq!(result, Err(PinCaptureStep::LockRead));
            }
            if matches!(case, 7 | 8 | 10 | 11) {
                assert_eq!(result, Err(PinCaptureStep::NativeLock));
            }
            let mut paths = reads.paths.into_inner();
            paths.sort();
            assert_eq!(
                paths,
                [
                    "/session/fixture-session/orientation",
                    "/wda/locked",
                    "/wda/screen"
                ]
            );
        }
    }

    #[tokio::test]
    async fn native_query_overlaps_async_reads_and_is_joined_after_read_failure() {
        let (native_started, wait_native) = tokio::sync::oneshot::channel();
        let (source_done, wait_source) = std::sync::mpsc::channel();
        let (source, lock, native) = tokio::time::timeout(
            Duration::from_secs(2),
            capture_reads(
                async {
                    wait_native.await.unwrap();
                    source_done.send(()).unwrap();
                    Err::<(), _>("source failed")
                },
                async { "lock read" },
                move || {
                    native_started.send(()).unwrap();
                    wait_source.recv_timeout(Duration::from_secs(1)).unwrap();
                    "native finished"
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(source, Err("source failed"));
        assert_eq!(lock, "lock read");
        assert_eq!(native.unwrap(), "native finished");
    }

    #[test]
    fn reported_wda_lock_keeps_false_distinct_from_unknown() {
        for (value, expected) in [
            (Some(true), "true"),
            (Some(false), "false"),
            (None, "unknown"),
        ] {
            assert_eq!(PinReportedLock::from_bool(value).as_str(), expected);
        }
    }

    #[test]
    fn preparation_diagnostic_retains_only_current_capture_timings() {
        let backend = bound_backend().with_input_admission(|| true).unwrap();
        let mut captures = 0;
        let report = backend.diagnose_preparation_with(|backend| {
            captures += 1;
            let now = Instant::now();
            backend.snapshot_with(
                now,
                |backend, _| {
                    assert_eq!(backend.capture_timings, PinCaptureTimings::default());
                    assert_eq!(backend.wda_reported_lock, PinReportedLock::Unknown);
                    if captures == 1 {
                        backend.capture_timings.native_ms = 101;
                        backend.wda_reported_lock = PinReportedLock::Unlocked;
                        Ok(normal_capture())
                    } else {
                        backend.capture_timings.source_ms = 7;
                        backend.capture_step = PinCaptureStep::SourceRead;
                        Err(BackendError::Unavailable)
                    }
                },
                || now,
            )
        });
        assert_eq!(report.outcome, "backend_unavailable");
        assert_eq!(report.phase, PinPhase::NormalConfirmation);
        assert_eq!(report.capture_step, PinCaptureStep::SourceRead);
        assert_eq!(report.timings.source_ms, 7);
        assert_eq!(report.timings.native_ms, 0);
        assert_eq!(report.wda_reported_lock, PinReportedLock::Unknown);
        assert!(report.snapshot.is_none());
    }

    #[test]
    fn preparation_diagnostic_stops_before_wake_reveal_or_any_digit() {
        for normal in [false, true] {
            let backend = bound_backend().with_input_admission(|| true).unwrap();
            let report = backend.diagnose_preparation_with(|backend| {
                assert!(!backend.wake_attempted);
                let now = Instant::now();
                backend.snapshot_with(
                    now,
                    |_, _| {
                        Ok(if normal {
                            normal_capture()
                        } else {
                            keypad_capture(false)
                        })
                    },
                    || now,
                )
            });
            assert_eq!(
                report.outcome,
                if normal {
                    "reveal_required"
                } else {
                    "empty_confirmed"
                }
            );
            assert_eq!(report.captures, 2);
            assert_eq!(
                report.phase,
                if normal {
                    PinPhase::RevealDispatch
                } else {
                    PinPhase::EmptyConfirmation
                }
            );
            assert_eq!(report.normal_locked, normal);
            assert_eq!(report.empty_locked, !normal);
            assert_eq!(report.capture_step, PinCaptureStep::Complete);
        }
    }

    #[test]
    fn preparation_diagnostic_retains_failure_step_and_refuses_drift() {
        let report = bound_backend()
            .with_input_admission(|| true)
            .unwrap()
            .diagnose_preparation_with(|backend| {
                backend.capture_step = PinCaptureStep::SourceRead;
                Err(BackendError::Unavailable)
            });
        assert_eq!(report.outcome, "backend_unavailable");
        assert_eq!(report.phase, PinPhase::InitialSnapshot);
        assert_eq!(report.capture_step, PinCaptureStep::SourceRead);
        assert_eq!(report.captures, 1);
        for denied in [false, true] {
            let backend = bound_backend()
                .with_input_admission(move || !denied)
                .unwrap();
            let mut captures = 0;
            let report = backend.diagnose_preparation_with(|backend| {
                captures += 1;
                let now = Instant::now();
                backend.snapshot_with(
                    now,
                    |_, _| {
                        Ok(if captures == 1 {
                            normal_capture()
                        } else {
                            keypad_capture(false)
                        })
                    },
                    || now,
                )
            });
            assert_eq!(report.outcome, "backend_unavailable");
            assert_eq!(
                report.phase,
                if denied {
                    PinPhase::OwnedAdmission
                } else {
                    PinPhase::NormalConfirmation
                }
            );
            assert_eq!(report.captures, if denied { 1 } else { 2 });
        }
    }

    #[test]
    fn prepare_keypad_only_uses_owned_inputs_and_retains_rejected_capture() {
        use std::cell::Cell;
        fn capture(
            backend: &mut NativePasscodeBackend,
            start: Instant,
            count: &Cell<usize>,
            mismatch: bool,
        ) -> Result<PasscodeScreen, BackendError> {
            let index = count.get() + 1;
            count.set(index);
            let at = start + Duration::from_millis(index as u64);
            backend.snapshot_with(
                at,
                |backend, _| {
                    let (screen, mut view) = if index < 4 || mismatch {
                        normal_capture()
                    } else {
                        keypad_capture(true)
                    };
                    if mismatch && index == 4 {
                        view.normal_lock.as_mut().unwrap().size.0 += 1.0;
                    }
                    backend.last_diagnostics = Some(PinDiagnostics {
                        native_locked: true,
                        wda_locked: true,
                        visible_locked: true,
                        prior_unlock: true,
                        normal_marker: view.normal_lock.is_some(),
                        ..Default::default()
                    });
                    Ok((screen, view))
                },
                || at,
            )
        }
        for mismatch in [false, true] {
            let backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            let captures = Cell::new(0);
            let mut inputs = Vec::new();
            let report = backend.prepare_keypad_only_with(
                |backend| capture(backend, start, &captures, mismatch),
                |backend| {
                    backend.prepare_owned_keypad_after_wake_with(
                        start,
                        |backend| capture(backend, start, &captures, mismatch),
                        |backend, action| {
                            backend.request_action_with(action, start, |_, action, _| {
                                inputs.push(match action {
                                    Action::Home => "home",
                                    Action::Swipe { .. } => "swipe",
                                    _ => panic!("preparation cannot send a digit"),
                                });
                                Ok(())
                            })
                        },
                        |_| {},
                    )
                },
            );
            assert_eq!(inputs, ["home", "swipe"]);
            assert_eq!(report.captures, if mismatch { 4 } else { 5 });
            assert!(report.snapshot.is_some());
            assert_eq!(
                report.outcome,
                if mismatch {
                    "backend_unavailable"
                } else {
                    "empty_confirmed"
                }
            );
            assert_eq!(
                report.refusal,
                if mismatch {
                    "normal_dimensions_changed"
                } else {
                    "none"
                }
            );
            assert_eq!(report.empty_locked, !mismatch);
        }
    }

    #[test]
    fn reveal_refusal_uses_only_fixed_capture_categories() {
        for (case, expected) in [
            (0, "lock_agreement"),
            (1, "alert"),
            (2, "nonempty"),
            (3, "normal_dimensions_changed"),
            (4, "normal_visibility_changed"),
            (5, "keypad_incomplete"),
            (6, "normal_unproven"),
        ] {
            let mut backend = bound_backend();
            let (screen, mut view) = normal_capture();
            let before = view.normal_lock.unwrap();
            let mut facts = PinDiagnostics {
                native_locked: true,
                wda_locked: true,
                visible_locked: true,
                prior_unlock: true,
                ..Default::default()
            };
            match case {
                0 => facts.native_locked = false,
                1 => facts.alert = true,
                2 => view.secure_field_value = Some("1 of 6 digits".into()),
                3 => view.normal_lock.as_mut().unwrap().size.0 += 1.0,
                4 => view.normal_lock.as_mut().unwrap().proud_visible ^= true,
                5 => {
                    view.normal_lock = None;
                    facts.field_seen = true;
                }
                6 => view.normal_lock = None,
                _ => unreachable!(),
            }
            let now = Instant::now();
            backend
                .snapshot_with(now, |_, _| Ok((screen, view)), || now)
                .unwrap();
            backend.last_diagnostics = Some(facts);
            assert_eq!(backend.reveal_refusal(before), expected);
        }
    }

    #[test]
    fn owned_wake_precedes_fresh_preparation_and_never_repeats() {
        use std::cell::{Cell, RefCell};
        for initially_normal in [false, true] {
            for needs_reveal in [false, true] {
                let mut backend = bound_backend().with_input_admission(|| true).unwrap();
                let start = Instant::now();
                let clock = Cell::new(start);
                let events = RefCell::new(Vec::new());
                backend
                    .snapshot_with(
                        start,
                        |_, _| {
                            Ok(if initially_normal {
                                normal_capture()
                            } else {
                                keypad_capture(true)
                            })
                        },
                        || start,
                    )
                    .unwrap();
                let mut captures = 0;
                backend
                    .prepare_owned_keypad_after_wake_with(
                        start,
                        |backend| {
                            captures += 1;
                            events.borrow_mut().push("capture");
                            clock.set(clock.get() + Duration::from_millis(1));
                            let at = clock.get();
                            backend.snapshot_with(
                                at,
                                |_, _| {
                                    Ok(if needs_reveal && captures < 3 {
                                        normal_capture()
                                    } else {
                                        keypad_capture(true)
                                    })
                                },
                                || at,
                            )
                        },
                        |backend, action| {
                            backend.request_action_with(action, clock.get(), |_, action, _| {
                                events.borrow_mut().push(match action {
                                    Action::Home => "home",
                                    Action::Swipe { .. } => "swipe",
                                    _ => panic!("preparation cannot dispatch a digit"),
                                });
                                Ok(())
                            })
                        },
                        |delay| {
                            assert_eq!(delay, Duration::from_millis(1000));
                            events.borrow_mut().push("settle");
                            clock.set(clock.get() + delay);
                        },
                    )
                    .unwrap();
                let expected = if needs_reveal {
                    vec!["home", "settle", "capture", "capture", "swipe", "capture"]
                } else {
                    vec!["home", "settle", "capture"]
                };
                assert_eq!(*events.borrow(), expected);
                assert!(backend.awaiting_first_digit);
                assert!(backend.last_view.as_ref().unwrap().empty_locked);
                assert!(
                    backend
                        .prepare_owned_keypad_after_wake_with(
                            clock.get(),
                            |_| panic!("no repeated capture"),
                            |_, _| panic!("no repeated Home or swipe"),
                            |_| panic!("no repeated settle"),
                        )
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn owned_wake_refuses_unsafe_initial_views_before_input() {
        for case in 0..8 {
            let mut backend = bound_backend()
                .with_input_admission(move || case != 6)
                .unwrap();
            let start = Instant::now();
            let (mut screen, mut view) = normal_capture();
            match case {
                0 => screen.native_lock = LockState::Unlocked,
                1 => {
                    screen.trust_prompt = true;
                    view.has_alert = true;
                }
                2 => {
                    (screen, view) = progress_capture(1);
                }
                3 => backend.session_bound = false,
                4 => backend.supervised_visibility_fallback = false,
                5 => backend.input_admission = None,
                6 | 7 => {}
                _ => unreachable!(),
            }
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .unwrap();
            let now = if case == 7 {
                start + PIN_VIEW_BUDGET
            } else {
                start
            };
            assert!(
                backend
                    .prepare_owned_keypad_after_wake_with(
                        now,
                        |_| panic!("no capture after refused initial admission"),
                        |_, _| panic!("no input after refused initial admission"),
                        |_| panic!("no settle after refused initial admission"),
                    )
                    .is_err()
            );
            assert!(!backend.wake_attempted);
        }
    }

    #[test]
    fn uncertain_owned_home_never_settles_reveals_or_repeats() {
        let mut backend = bound_backend().with_input_admission(|| true).unwrap();
        let start = Instant::now();
        backend
            .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
            .unwrap();
        let mut writes = 0;
        assert!(
            backend
                .prepare_owned_keypad_after_wake_with(
                    start,
                    |_| panic!("no capture after uncertain Home"),
                    |backend, action| backend.request_action_with(action, start, |_, action, _| {
                        assert!(matches!(action, Action::Home));
                        writes += 1;
                        Err(BackendError::Unavailable)
                    }),
                    |_| panic!("no settle after uncertain Home"),
                )
                .is_err()
        );
        assert_eq!(writes, 1);
        assert_eq!(backend.phase(), PinPhase::WakeDispatch);
        assert!(backend.last_view.is_none());
        assert!(
            backend
                .prepare_owned_keypad_after_wake_with(
                    start,
                    |_| panic!("no repeated capture"),
                    |_, _| panic!("no repeated input"),
                    |_| panic!("no repeated settle"),
                )
                .is_err()
        );
        assert!(
            backend
                .press_digit_with(0, |_, _| panic!("no digit after uncertain Home"))
                .is_err()
        );
    }

    #[test]
    fn owned_wake_refuses_postcapture_drift_before_reveal_or_digits() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for case in 0..9 {
            let owned = Arc::new(AtomicBool::new(true));
            let admission = Arc::clone(&owned);
            let mut backend = bound_backend()
                .with_input_admission(move || admission.load(Ordering::SeqCst))
                .unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let mut writes = 0;
            let mut captures = 0;
            assert!(
                backend
                    .prepare_owned_keypad_after_wake_with(
                        start,
                        |backend| {
                            captures += 1;
                            if case < 2 {
                                backend.capture_step = if case == 0 {
                                    PinCaptureStep::SourceRead
                                } else {
                                    PinCaptureStep::SessionAfter
                                };
                                return Err(BackendError::Unavailable);
                            }
                            let (mut screen, mut view) =
                                progress_capture(if case == 2 { 1 } else { 0 });
                            match case {
                                3 => screen.native_lock = LockState::Unlocked,
                                4 => screen.wda_lock = LockState::Uncertain,
                                5 => {
                                    screen.trust_prompt = true;
                                    view.has_alert = true;
                                }
                                _ => {}
                            }
                            let at = if case == 7 {
                                start
                            } else {
                                start + Duration::from_millis(1001)
                            };
                            let result = backend.snapshot_with(
                                at,
                                |_, _| Ok((screen, view)),
                                || {
                                    at + if case == 6 {
                                        PIN_VIEW_BUDGET
                                    } else {
                                        Duration::ZERO
                                    }
                                },
                            );
                            if case == 8 {
                                owned.store(false, Ordering::SeqCst);
                            }
                            result
                        },
                        |backend, action| backend.request_action_with(
                            action,
                            start,
                            |_, action, _| {
                                assert!(matches!(action, Action::Home));
                                writes += 1;
                                Ok(())
                            }
                        ),
                        |_| {},
                    )
                    .is_err()
            );
            assert_eq!((writes, captures), (1, 1), "case {case}");
            assert!(!backend.awaiting_first_digit);
            assert!(backend.last_view.is_none());
            assert!(
                backend
                    .press_digit_with(0, |_, _| panic!("no digit after drift"))
                    .is_err()
            );
        }
    }

    #[test]
    fn owned_empty_keypads_need_no_input_and_require_zero_until_first_digit() {
        for hidden in [false, true] {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(keypad_capture(hidden)), || start)
                .unwrap();
            assert_eq!(
                backend.prepare_owned_keypad_with(
                    start,
                    |_| panic!("already empty: no extra preparation capture"),
                    |_, _| panic!("already empty: no swipe or wake")
                ),
                Ok(())
            );
            assert!(backend.awaiting_first_digit);
            let (mut screen, mut view) = keypad_capture(hidden);
            screen.secure_field_value = Some("1 of 6 digits".into());
            view.secure_field_value = screen.secure_field_value.clone();
            assert!(
                backend
                    .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                    .is_err()
            );
            assert!(backend.last_view.is_none());
        }
    }

    #[test]
    fn owned_normal_lock_reveals_once_then_requires_empty_keypad() {
        for coordinate in [false, true] {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            backend.coordinate_pin_taps = coordinate;
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let mut captures = 0;
            let mut writes = 0;
            assert_eq!(
                backend.prepare_owned_keypad_with(
                    start,
                    |backend| {
                        captures += 1;
                        assert_eq!(
                            backend.phase(),
                            if captures == 1 {
                                PinPhase::NormalConfirmation
                            } else {
                                PinPhase::RevealPostcapture
                            }
                        );
                        let at = start + Duration::from_millis(captures);
                        backend.snapshot_with(
                            at,
                            |_, _| {
                                Ok(if captures == 1 {
                                    normal_capture()
                                } else {
                                    keypad_capture(false)
                                })
                            },
                            || at,
                        )
                    },
                    |backend, action| {
                        writes += 1;
                        assert_eq!(backend.phase(), PinPhase::RevealDispatch);
                        let Action::Swipe {
                            from,
                            to,
                            duration_ms,
                        } = action
                        else {
                            panic!("reveal must be a single swipe");
                        };
                        assert_eq!(
                            (from.x, from.y, to.x, to.y, *duration_ms),
                            (200.0, 792.0, 200.0, 360.0, 300)
                        );
                        backend.request_action_with(
                            action,
                            start + Duration::from_millis(1),
                            |_, _, deadline| {
                                assert_eq!(
                                    deadline,
                                    start + Duration::from_millis(1) + PIN_VIEW_BUDGET
                                );
                                Ok(())
                            },
                        )
                    }
                ),
                Ok(())
            );
            assert_eq!((captures, writes), (2, 1));
            assert_eq!(backend.phase(), PinPhase::EmptyConfirmation);
            assert!(backend.awaiting_first_digit);
            assert!(backend.last_view.as_ref().unwrap().empty_locked);
            assert!(
                backend
                    .prepare_owned_keypad_with(
                        start,
                        |_| panic!("no repeat capture"),
                        |_, _| panic!("no reswipe")
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn owned_reveal_settles_visibility_changes_but_not_dimensions() {
        for end in 0..3 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let mut captures = 0;
            let mut writes = 0;
            let result = backend.prepare_owned_keypad_with(
                start,
                |backend| {
                    captures += 1;
                    let at = start + Duration::from_millis(captures);
                    backend.snapshot_with(
                        at,
                        |_, _| {
                            Ok(if captures < 3 || end != 0 {
                                let (screen, mut view) = normal_capture();
                                if captures >= 2 {
                                    view.normal_lock.as_mut().unwrap().proud_visible ^= true;
                                    if end == 2 {
                                        view.normal_lock.as_mut().unwrap().size.0 += 1.0;
                                    }
                                }
                                (screen, view)
                            } else {
                                keypad_capture(true)
                            })
                        },
                        || at,
                    )
                },
                |backend, action| {
                    writes += 1;
                    backend.request_action_with(
                        action,
                        start + Duration::from_millis(1),
                        |_, _, _| Ok(()),
                    )
                },
            );
            assert_eq!(result.is_ok(), end == 0);
            assert_eq!((writes, captures), (1, [3, 4, 2][end]));
            assert_eq!(backend.awaiting_first_digit, end == 0);
            if end == 0 {
                assert_eq!(backend.phase(), PinPhase::EmptyConfirmation);
                assert!(backend.last_view.as_ref().unwrap().empty_locked);
            } else {
                assert_eq!(backend.phase(), PinPhase::RevealPostcapture);
                assert_eq!(
                    backend.capture_step(),
                    if end == 1 {
                        PinCaptureStep::RevealSettleLimit
                    } else {
                        PinCaptureStep::EmptyAdmission
                    }
                );
            }
        }
    }

    #[test]
    fn reveal_initial_settle_precedes_capture_and_rechecks_admission_budget() {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicBool, Ordering};
        for case in 0..3 {
            let paused = Arc::new(AtomicBool::new(false));
            let admission_pause = Arc::clone(&paused);
            let mut backend = bound_backend()
                .with_input_admission(move || {
                    assert!(admission_pause.load(Ordering::SeqCst));
                    case != 1
                })
                .unwrap();
            let start = Instant::now();
            let clock = Cell::new(start);
            let mut captures = 0;
            let result = backend.settle_owned_reveal_with(
                normal_capture().1.normal_lock.unwrap(),
                start,
                |backend| {
                    captures += 1;
                    assert!(paused.load(Ordering::SeqCst));
                    assert_eq!(clock.get().duration_since(start), REVEAL_INITIAL_SETTLE);
                    let at = clock.get();
                    backend.snapshot_with(at, |_, _| Ok(keypad_capture(false)), || at)
                },
                || clock.get(),
                |delay| {
                    assert!(!paused.swap(true, Ordering::SeqCst));
                    assert_eq!(delay, Duration::from_millis(1500));
                    clock.set(
                        start
                            + if case == 2 {
                                Duration::from_secs(6)
                            } else {
                                delay
                            },
                    );
                },
            );
            assert_eq!(result.is_ok(), case == 0);
            assert_eq!(captures, usize::from(case == 0));
            assert_eq!(backend.awaiting_first_digit, case == 0);
            assert!(paused.load(Ordering::SeqCst));
            if case == 2 {
                assert_eq!(backend.capture_step(), PinCaptureStep::RevealSettleLimit);
            }
        }
    }

    #[test]
    fn reveal_settling_has_capture_and_overall_limits() {
        use std::cell::Cell;
        for capture_ms in [1, 3100] {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            let clock = Cell::new(start);
            let mut captures = 0;
            let result = backend.settle_owned_reveal_with(
                normal_capture().1.normal_lock.unwrap(),
                start,
                |backend| {
                    captures += 1;
                    let at = clock.get() + Duration::from_nanos(1);
                    clock.set(at + Duration::from_millis(capture_ms));
                    backend.snapshot_with(at, |_, _| Ok(normal_capture()), || clock.get())
                },
                || clock.get(),
                |delay| clock.set(clock.get() + delay),
            );
            assert!(result.is_err());
            assert_eq!(captures, if capture_ms == 1 { 3 } else { 2 });
            assert_eq!(backend.capture_step(), PinCaptureStep::RevealSettleLimit);
            assert!(!backend.awaiting_first_digit);
            assert!(clock.get().duration_since(start) < REVEAL_SETTLE_BUDGET);
        }
    }

    #[test]
    fn reveal_settling_reserves_capture_budget_after_ownership_validation() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let elapsed_ms = Arc::new(AtomicU64::new(0));
        let admission_clock = Arc::clone(&elapsed_ms);
        let mut backend = bound_backend()
            .with_input_admission(move || {
                admission_clock.fetch_add(6000, Ordering::SeqCst);
                true
            })
            .unwrap();
        let start = Instant::now();
        let result = backend.settle_owned_reveal_with(
            normal_capture().1.normal_lock.unwrap(),
            start,
            |_| panic!("ownership validation consumed the capture reservation"),
            || start + Duration::from_millis(elapsed_ms.load(Ordering::SeqCst)),
            |delay| {
                assert_eq!(delay, REVEAL_INITIAL_SETTLE);
                elapsed_ms.fetch_add(delay.as_millis() as u64, Ordering::SeqCst);
            },
        );
        assert!(result.is_err());
        assert_eq!(backend.capture_step(), PinCaptureStep::RevealSettleLimit);
        assert!(!backend.awaiting_first_digit);
    }

    #[test]
    fn reveal_settling_never_retries_capture_errors_or_unsafe_views() {
        use std::cell::Cell;
        for case in 0..10 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            let clock = Cell::new(start);
            let mut captures = 0;
            let paused = Cell::new(false);
            let result = backend.settle_owned_reveal_with(
                normal_capture().1.normal_lock.unwrap(),
                start,
                |backend| {
                    captures += 1;
                    if matches!(case, 5 | 7) {
                        backend.capture_step = if case == 5 {
                            PinCaptureStep::SourceRead
                        } else {
                            PinCaptureStep::SessionAfter
                        };
                        return Err(BackendError::Unavailable);
                    }
                    let (mut screen, mut view) = if case == 4 {
                        normal_capture()
                    } else {
                        keypad_capture(false)
                    };
                    match case {
                        0 => {
                            screen.secure_field_value = Some("1 of 6 digits".into());
                            view.secure_field_value = screen.secure_field_value.clone();
                        }
                        1 => {
                            screen.trust_prompt = true;
                            view.has_alert = true;
                        }
                        2 => screen.native_lock = LockState::Unlocked,
                        3 => screen.wda_lock = LockState::Uncertain,
                        4 => view.normal_lock.as_mut().unwrap().size.0 += 1.0,
                        6 | 9 => {}
                        8 => screen.native_lock = LockState::Uncertain,
                        _ => unreachable!(),
                    }
                    let at = if case == 9 {
                        start
                    } else {
                        start + Duration::from_nanos(1)
                    };
                    clock.set(
                        at + if case == 6 {
                            PIN_VIEW_BUDGET
                        } else {
                            Duration::from_millis(1)
                        },
                    );
                    backend.snapshot_with(at, |_, _| Ok((screen, view)), || clock.get())
                },
                || clock.get(),
                |delay| {
                    assert!(!paused.replace(true), "unsafe capture cannot be retried");
                    assert_eq!(delay, REVEAL_INITIAL_SETTLE);
                },
            );
            assert!(result.is_err(), "case {case}");
            assert_eq!(captures, 1, "case {case}");
            assert!(!backend.awaiting_first_digit);
            if case == 6 {
                assert_eq!(backend.capture_step(), PinCaptureStep::Freshness);
            }
        }
    }

    #[test]
    fn reveal_settling_rechecks_ownership_after_capture() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let owned = Arc::new(AtomicBool::new(true));
        let admit = Arc::clone(&owned);
        let mut backend = bound_backend()
            .with_input_admission(move || admit.load(Ordering::SeqCst))
            .unwrap();
        let start = Instant::now();
        let result = backend.settle_owned_reveal_with(
            normal_capture().1.normal_lock.unwrap(),
            start,
            |backend| {
                let at = start + Duration::from_nanos(1);
                let result = backend.snapshot_with(at, |_, _| Ok(keypad_capture(false)), || at);
                owned.store(false, Ordering::SeqCst);
                result
            },
            Instant::now,
            |delay| assert_eq!(delay, REVEAL_INITIAL_SETTLE),
        );
        assert!(result.is_err());
        assert!(!backend.awaiting_first_digit);
    }

    #[test]
    fn owned_reveal_refuses_changed_reused_or_unowned_preparation_before_writes() {
        for case in 0..13 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            let (mut screen, mut view) = normal_capture();
            match case {
                0 => screen.connected_usb = HardwareId::new("other-phone").unwrap(),
                1 => screen.wda_target = HardwareId::new("other-phone").unwrap(),
                2 => screen.native_lock = LockState::Uncertain,
                3 => screen.wda_lock = LockState::Uncertain,
                4 => screen.visible_lock = LockState::Uncertain,
                5 => screen.before_first_unlock = true,
                6 => screen.trust_prompt = true,
                7 => view.normal_lock = None,
                _ => (),
            }
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .unwrap();
            if case == 8 {
                backend.input_admission = Some(Arc::new(|| false));
            }
            let now = if case == 9 {
                start + PIN_VIEW_BUDGET
            } else {
                start
            };
            assert!(
                backend
                    .prepare_owned_keypad_with(
                        now,
                        |backend| {
                            assert!(case >= 10);
                            let (screen, mut view) = normal_capture();
                            if case == 10 {
                                view.normal_lock.as_mut().unwrap().size.0 += 1.0;
                            }
                            if case == 12 {
                                view.normal_lock.as_mut().unwrap().proud_visible = false;
                            }
                            let at = if case == 11 {
                                start
                            } else {
                                start + Duration::from_millis(1)
                            };
                            backend.snapshot_with(at, |_, _| Ok((screen, view)), || at)
                        },
                        |_, _| panic!("unsafe preparation: zero writes")
                    )
                    .is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn uncertain_reveal_never_reswipes_or_permits_a_digit() {
        for case in 0..4 {
            let mut backend = bound_backend().with_input_admission(|| true).unwrap();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(normal_capture()), || start)
                .unwrap();
            let mut captures = 0;
            let mut writes = 0;
            assert!(
                backend
                    .prepare_owned_keypad_with(
                        start,
                        |backend| {
                            captures += 1;
                            if captures == 2 && case == 1 {
                                return Err(BackendError::Unavailable);
                            }
                            let at = start + Duration::from_millis(captures);
                            let (mut screen, mut view) = if captures == 1 || case == 2 {
                                normal_capture()
                            } else {
                                keypad_capture(false)
                            };
                            if captures == 2 && case == 3 {
                                screen.secure_field_value = Some("1 of 6 digits".into());
                                view.secure_field_value = screen.secure_field_value.clone();
                            }
                            backend.snapshot_with(at, |_, _| Ok((screen, view)), || at)
                        },
                        |backend, action| {
                            writes += 1;
                            backend.request_action_with(
                                action,
                                start + Duration::from_millis(1),
                                |_, _, _| {
                                    if case == 0 {
                                        Err(BackendError::Unavailable)
                                    } else {
                                        Ok(())
                                    }
                                },
                            )
                        }
                    )
                    .is_err()
            );
            assert_eq!(writes, 1);
            assert_eq!(
                backend.phase(),
                if case == 0 {
                    PinPhase::RevealDispatch
                } else {
                    PinPhase::RevealPostcapture
                }
            );
            assert_eq!(backend.progress_check, ProgressCheck::Failed);
            assert!(backend.last_view.is_none());
            assert!(
                backend
                    .prepare_owned_keypad_with(
                        start,
                        |_| panic!("terminal"),
                        |_, _| panic!("no reswipe")
                    )
                    .is_err()
            );
            assert!(
                backend
                    .press_digit_with(0, |_, _| panic!("no digit"))
                    .is_err()
            );
        }
    }

    #[test]
    fn visible_opt_in_and_existing_default_keypads_do_not_wake() {
        for (mut backend, hidden) in [
            (bound_backend(), false),
            (backend(), false),
            (
                NativePasscodeBackend::new_with_supervised_visibility_fallback(
                    HardwareId::new("fixture-phone").unwrap(),
                    true,
                )
                .unwrap(),
                true,
            ),
        ] {
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(keypad_capture(hidden)), || start)
                .unwrap();
            assert_eq!(backend.show_keypad(), Ok(()));
            assert!(!backend.awaiting_first_digit);
            assert!(backend.last_view.is_some());
        }
        let mut backend = backend();
        let start = Instant::now();
        let (mut screen, view) = keypad_capture(false);
        screen.secure_field_value = Some("1 of 6 digits".into());
        assert!(
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .is_ok()
        );
        assert_eq!(backend.show_keypad(), Ok(()));
    }

    #[test]
    fn supervised_bound_wake_consumes_original_deadline_and_settles_once() {
        let mut backend = bound_backend();
        let start = Instant::now();
        backend
            .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
            .unwrap();
        let mut wakes = 0;
        let mut settles = 0;
        let mut captures = 0;
        assert_eq!(
            backend.wake_hidden_with(
                start + Duration::from_millis(100),
                |backend, deadline| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(deadline, start + PIN_VIEW_BUDGET);
                    wakes += 1;
                    Ok(())
                },
                |delay| {
                    assert_eq!(delay, Duration::from_millis(1500));
                    settles += 1;
                },
                |backend| {
                    captures += 1;
                    backend.snapshot_with(
                        start + SUPERVISED_WAKE_SETTLE,
                        |_, _| Ok(hidden_capture()),
                        || start + SUPERVISED_WAKE_SETTLE,
                    )
                }
            ),
            Ok(())
        );
        assert_eq!((wakes, settles, captures), (1, 1, 1));
        assert!(backend.awaiting_first_digit);
        assert_eq!(
            backend.last_view.as_ref().unwrap().fresh_until,
            start + SUPERVISED_WAKE_SETTLE + PIN_VIEW_BUDGET
        );
    }

    #[test]
    fn hidden_wake_requires_opt_in_fresh_empty_locked_evidence() {
        type Change = fn(&mut PasscodeScreen, &mut ParsedPasscodeView);
        for change in [
            (|s: &mut PasscodeScreen, _: &mut ParsedPasscodeView| {
                s.wda_target = HardwareId::new("other").unwrap()
            }) as Change,
            |s, _| s.native_lock = LockState::Unlocked,
            |s, _| s.visible_lock = LockState::Uncertain,
            |s, _| s.wda_lock = LockState::Uncertain,
            |s, _| s.before_first_unlock = true,
            |s, _| s.trust_prompt = true,
            |s, _| s.keypad_ready = false,
            |s, _| s.secure_field_value = Some("1 of 6 digits".into()),
            |_, v| v.secure_field_value = Some("0 of 4 digits".into()),
            |_, v| v.has_alert = true,
            |_, v| v.visibility_fallback = false,
            |_, v| v.layout_valid = false,
        ] {
            let mut backend = bound_backend();
            let start = Instant::now();
            let (mut screen, mut view) = hidden_capture();
            change(&mut screen, &mut view);
            backend
                .snapshot_with(start, |_, _| Ok((screen, view)), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start,
                    |_, _| panic!("no wake"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
            assert!(backend.last_view.is_none());
        }
        for elapsed in [PIN_VIEW_BUDGET, PIN_VIEW_BUDGET + Duration::from_nanos(1)] {
            let mut backend = bound_backend();
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start + elapsed,
                    |_, _| panic!("expired view"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
            assert!(backend.last_view.is_none());
        }
        for supervised in [false, true] {
            let mut backend = backend();
            backend.supervised_visibility_fallback = supervised;
            let start = Instant::now();
            backend
                .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
                .unwrap();
            assert_eq!(
                backend.wake_hidden_with(
                    start,
                    |_, _| panic!("unbound path"),
                    |_| panic!("no settle"),
                    |_| panic!("no capture")
                ),
                Err(BackendError::Unavailable)
            );
        }
    }

    #[test]
    fn uncertain_wake_has_no_settle_reobserve_or_fallback() {
        let mut backend = bound_backend();
        let start = Instant::now();
        backend
            .snapshot_with(start, |_, _| Ok(hidden_capture()), || start)
            .unwrap();
        let mut wakes = 0;
        assert_eq!(
            backend.wake_hidden_with(
                start,
                |_, _| {
                    wakes += 1;
                    Err(BackendError::Unavailable)
                },
                |_| panic!("no settle"),
                |_| panic!("no capture")
            ),
            Err(BackendError::Unavailable)
        );
        assert_eq!(wakes, 1);
        assert!(backend.last_view.is_none());
        assert_eq!(
            backend.wake_hidden_with(
                start,
                |_, _| panic!("no replay"),
                |_| panic!("no settle"),
                |_| panic!("no capture")
            ),
            Err(BackendError::Unavailable)
        );
    }

    #[test]
    fn settled_and_core_pre_mark_snapshots_must_still_be_empty_and_locked() {
        use handkite_core::passcode::Passcode;
        use handkite_core::protocol::{DeviceBinding, DeviceKey};
        use handkite_core::unlock::{AttemptGate, GateError, UnlockPolicy, unlock_once};
        struct Harness {
            backend: NativePasscodeBackend,
            start: Instant,
            fail_settled: bool,
            samples: usize,
            wakes: usize,
        }
        impl PasscodeBackend for Harness {
            fn snapshot(&mut self) -> Result<PasscodeScreen, BackendError> {
                let (mut screen, view) = hidden_capture();
                if self.samples > 0 {
                    screen.wda_lock = LockState::Uncertain;
                }
                self.samples += 1;
                self.backend
                    .snapshot_with(self.start, |_, _| Ok((screen, view)), || self.start)
            }
            fn show_keypad(&mut self) -> Result<(), BackendError> {
                let fail = self.fail_settled;
                let start = self.start + SUPERVISED_WAKE_SETTLE;
                let wakes = &mut self.wakes;
                self.backend.wake_hidden_with(
                    self.start,
                    |_, _| {
                        *wakes += 1;
                        Ok(())
                    },
                    |_| {},
                    |backend| {
                        let (mut screen, view) = hidden_capture();
                        if fail {
                            screen.secure_field_value = Some("1 of 6 digits".into());
                        }
                        backend.snapshot_with(start, |_, _| Ok((screen, view)), || start)
                    },
                )
            }
            fn press_digit(&mut self, _: u8) -> Result<(), BackendError> {
                panic!("no digit submission")
            }
            fn delete_digit(&mut self) -> Result<(), BackendError> {
                panic!("no deletion")
            }
        }
        struct Gate;
        impl AttemptGate for Gate {
            fn is_disabled(&self) -> Result<bool, GateError> {
                Ok(false)
            }
            fn mark_uncertain(&mut self) -> Result<(), GateError> {
                panic!("preflight must not mark")
            }
            fn clear(&mut self) -> Result<(), GateError> {
                panic!("preflight must not clear")
            }
        }
        for fail_settled in [false, true] {
            let mut harness = Harness {
                backend: bound_backend(),
                start: Instant::now(),
                fail_settled,
                samples: 0,
                wakes: 0,
            };
            let binding = DeviceBinding {
                key: DeviceKey::new("iphone").unwrap(),
                usb_id: HardwareId::new("fixture-phone").unwrap(),
            };
            // A synthetic code is required by the core interface but never read or submitted.
            let code = Passcode::new("0".repeat(6)).unwrap();
            assert!(
                unlock_once(
                    &mut harness,
                    &mut Gate,
                    &binding,
                    &binding.key,
                    &code,
                    UnlockPolicy::default()
                )
                .is_err()
            );
            assert_eq!(harness.wakes, 1);
            assert!(harness.backend.last_view.is_none());
        }
    }

    #[test]
    fn snapshot_deadline_starts_before_capture_and_rejects_expiry_at_equality() {
        for elapsed in [
            Duration::ZERO,
            PIN_VIEW_BUDGET - Duration::from_nanos(1),
            PIN_VIEW_BUDGET,
            PIN_VIEW_BUDGET + Duration::from_secs(1),
        ] {
            let mut backend = backend();
            let started = Instant::now();
            prepare(&mut backend, started);
            let result = backend.snapshot_with(
                started,
                |backend, capture_started| {
                    assert!(backend.last_view.is_none());
                    assert_eq!(capture_started, started);
                    Ok(capture())
                },
                || started + elapsed,
            );
            if elapsed < PIN_VIEW_BUDGET {
                assert!(result.is_ok());
                let prepared = backend.last_view.as_ref().unwrap();
                assert_eq!(prepared.fresh_until, started + PIN_VIEW_BUDGET);
                assert_eq!(prepared.view.screen_size, Some((400.0, 800.0)));
            } else {
                assert!(matches!(result, Err(BackendError::Unavailable)));
                assert!(backend.last_view.is_none());
            }
        }
    }

    #[test]
    fn snapshot_failure_discards_prior_view_before_capture() {
        for error in [BackendError::Unavailable, BackendError::Disconnected] {
            let mut backend = backend();
            let started = Instant::now();
            prepare(&mut backend, started);
            let result = backend.snapshot_with(
                started,
                |backend, _| {
                    assert!(backend.last_view.is_none());
                    Err(error)
                },
                || panic!("failed capture cannot prepare a view"),
            );
            assert!(matches!(result, Err(actual) if actual == error));
            assert!(backend.last_view.is_none());
            assert_eq!(
                backend.request_action_with(&Action::Home, started, |_, _, _| {
                    panic!("failed capture cannot dispatch stale input")
                }),
                Err(BackendError::Unavailable)
            );
        }
    }

    #[test]
    fn dispatch_consumes_view_and_preserves_original_deadline_on_every_result() {
        for coordinate in [false, true] {
            for action in [
                Action::Tap {
                    at: Point { x: 80.0, y: 340.0 },
                },
                Action::Tap {
                    at: Point { x: 280.0, y: 640.0 },
                },
                keypad_swipe((400.0, 800.0)).unwrap(),
            ] {
                for result in [
                    Ok(()),
                    Err(BackendError::Unavailable),
                    Err(BackendError::Disconnected),
                ] {
                    let mut backend = if coordinate {
                        bound_backend().with_coordinate_pin_taps().unwrap()
                    } else {
                        backend()
                    };
                    let started = Instant::now();
                    backend
                        .snapshot_with(
                            started,
                            |_, _| Ok(capture()),
                            || started + Duration::from_secs(4),
                        )
                        .unwrap();
                    let mut dispatches = 0;
                    assert_eq!(
                        backend.request_action_with(
                            &action,
                            started + Duration::from_millis(4_500),
                            |backend, sent, fresh_until| {
                                assert!(backend.last_view.is_none());
                                assert!(std::ptr::eq(sent, &action));
                                assert_eq!(fresh_until, started + PIN_VIEW_BUDGET);
                                dispatches += 1;
                                result
                            },
                        ),
                        result
                    );
                    assert_eq!(dispatches, 1);
                    assert!(backend.last_view.is_none());
                    assert_eq!(
                        backend.request_action_with(&action, started, |_, _, _| {
                            panic!("consumed input cannot be retried")
                        }),
                        Err(BackendError::Unavailable)
                    );
                }
            }
        }
    }

    #[test]
    fn missing_or_expired_view_never_dispatches() {
        for coordinate in [false, true] {
            let action = Action::Tap {
                at: Point { x: 80.0, y: 340.0 },
            };
            let mut backend = if coordinate {
                bound_backend().with_coordinate_pin_taps().unwrap()
            } else {
                backend()
            };
            let started = Instant::now();
            assert_eq!(
                backend.request_action_with(&action, started, |_, _, _| panic!("missing view")),
                Err(BackendError::Unavailable)
            );
            for elapsed in [
                PIN_VIEW_BUDGET - Duration::from_nanos(1),
                PIN_VIEW_BUDGET,
                PIN_VIEW_BUDGET + Duration::from_nanos(1),
            ] {
                prepare(&mut backend, started);
                let mut dispatches = 0;
                let result = backend.request_action_with(&action, started + elapsed, |_, _, _| {
                    dispatches += 1;
                    Ok(())
                });
                if elapsed < PIN_VIEW_BUDGET {
                    assert_eq!(result, Ok(()));
                    assert_eq!(dispatches, 1);
                } else {
                    assert_eq!(result, Err(BackendError::Unavailable));
                    assert_eq!(dispatches, 0);
                }
                assert!(backend.last_view.is_none());
            }
        }
    }

    #[test]
    fn swipe_stays_inside_valid_phone_geometry() {
        let Action::Swipe {
            from,
            to,
            duration_ms,
        } = keypad_swipe((393.0, 852.0)).unwrap()
        else {
            panic!("expected swipe");
        };
        assert_eq!(from.x, 196.5);
        assert_eq!(from.y, 844.0);
        assert!(from.y > to.y);
        assert_eq!(duration_ms, 300);
        assert!(keypad_swipe((0.0, 852.0)).is_err());
    }
}
