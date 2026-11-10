use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{Action, HardwareId, LockState, Point};
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_action::{CoordinateTapError, WdaActions};
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_http::HttpResponse;
use handkite_host::wda_runner::OwnedWdaRunner;
use handkite_host::wda_session::{
    SessionClose, close_owned_session, with_owned_session_idle_settings_receipt,
};
use handkite_host::wda_settings::{
    IdleAction, IdleSettingsOutcome, Restoration, with_temporary_idle_settings,
};
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, install_config, native_lock};
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLOCK: &str = "com.apple.mobiletimer";
const MAX_CAPTURE_MS: u64 = 10_000;
const MAX_AGE_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefusalReason {
    NativeLocked,
    NativeLockUnproven,
    WdaLocked,
    WdaLockUnproven,
    ForegroundNotClock,
    SessionUnproven,
    IdentityUnproven,
    FreshnessUnproven,
    CaptureFailed,
    WorldClockAttributes,
    StopwatchAttributes,
    GeometryMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrialError {
    Refused(RefusalReason),
    Unknown,
    CoordinateUnknown(CoordinateUnknown),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinateUnknown {
    Http404,
    ReceiptUnproven,
    PostconditionUnproven,
}

impl TrialError {
    fn message(self) -> &'static str {
        match self {
            Self::Refused(RefusalReason::NativeLocked) => "refused reason=native_locked",
            Self::Refused(RefusalReason::NativeLockUnproven) => {
                "refused reason=native_lock_unproven"
            }
            Self::Refused(RefusalReason::WdaLocked) => "refused reason=wda_locked",
            Self::Refused(RefusalReason::WdaLockUnproven) => "refused reason=wda_lock_unproven",
            Self::Refused(RefusalReason::ForegroundNotClock) => {
                "refused reason=foreground_not_clock"
            }
            Self::Refused(RefusalReason::SessionUnproven) => "refused reason=session_unproven",
            Self::Refused(RefusalReason::IdentityUnproven) => "refused reason=identity_unproven",
            Self::Refused(RefusalReason::FreshnessUnproven) => "refused reason=freshness_unproven",
            Self::Refused(RefusalReason::CaptureFailed) => "refused reason=capture_failed",
            Self::Refused(RefusalReason::WorldClockAttributes) => {
                "refused reason=world_clock_attributes"
            }
            Self::Refused(RefusalReason::StopwatchAttributes) => {
                "refused reason=stopwatch_attributes"
            }
            Self::Refused(RefusalReason::GeometryMismatch) => "refused reason=geometry_mismatch",
            Self::Unknown => "unknown",
            Self::CoordinateUnknown(CoordinateUnknown::Http404) => "unknown reason=http_404",
            Self::CoordinateUnknown(CoordinateUnknown::ReceiptUnproven) => {
                "unknown reason=receipt_unproven"
            }
            Self::CoordinateUnknown(CoordinateUnknown::PostconditionUnproven) => {
                "unknown reason=postcondition_unproven"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Transport {
    W3c,
    Coordinate,
}

impl Transport {
    fn unknown(self, reason: CoordinateUnknown) -> TrialError {
        match self {
            Self::W3c => TrialError::Unknown,
            Self::Coordinate => TrialError::CoordinateUnknown(reason),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::W3c => "w3c",
            Self::Coordinate => "coordinate",
        }
    }
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let (expected, transport) = options(args)?;
    let mut live =
        Live::new(expected.clone()).map_err(|reason| TrialError::Refused(reason).message())?;
    run(&mut live, &expected, transport).map_err(TrialError::message)?;
    println!("touch_trial=confirmed transport={}", transport.name());
    Ok(())
}

fn options(args: &[String]) -> Result<(HardwareId, Transport), &'static str> {
    let (config, transport) = option_syntax(args)?;
    let binding = host_config::load(config).map_err(|_| "configuration_invalid")?;
    Ok((binding.usb_id, transport))
}

fn option_syntax(args: &[String]) -> Result<(&Path, Transport), &'static str> {
    let [config_flag, config, transport_flag, transport] = args else {
        return Err("invalid_arguments");
    };
    if config_flag != "--config" || transport_flag != "--transport" {
        return Err("invalid_arguments");
    }
    let transport = match transport.as_str() {
        "w3c" => Transport::W3c,
        "coordinate" => Transport::Coordinate,
        _ => return Err("invalid_arguments"),
    };
    Ok((Path::new(config), transport))
}

pub(super) fn idle_command(args: &[String]) -> Result<(), &'static str> {
    settings_command(args)
}

pub(super) fn owned_command(args: &[String]) -> Result<(), &'static str> {
    let (path, transport) = option_syntax(args)?;
    let layout = ServiceLayout::current_account().map_err(|_| "refused reason=service_layout")?;
    if path != layout.config_file() {
        return Err("refused reason=protected_config_required");
    }
    let owner =
        Arc::new(ServiceLock::acquire(layout).map_err(|_| "refused reason=service_ownership")?);
    let config = install_config::load_protected_config(owner.layout())
        .map_err(|_| "refused reason=protected_config_unavailable")?;
    let runner_id = config
        .wda_runner_bundle_id()
        .ok_or("refused reason=runner_unconfigured")?;
    let expected = config.binding().usb_id.clone();
    owner
        .validate_current()
        .map_err(|_| "refused reason=service_ownership")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "refused reason=runner_unavailable")?;
    let result = runtime.block_on(async {
        let usb = PinnedUsb::new(expected.clone());
        let runner = OwnedWdaRunner::start(usb.clone(), runner_id, Duration::from_secs(120))
            .await
            .map_err(|_| "refused reason=runner_unavailable")?;
        let liveness = runner.liveness();
        let wda = UsbWda::new(usb);
        let admit = || {
            if liveness.is_finished() || owner.validate_current().is_err() {
                Err(())
            } else {
                Ok(())
            }
        };
        let action = |worker_session: String| {
            let worker_owner = Arc::clone(&owner);
            let worker_liveness = liveness.clone();
            async move {
                if worker_liveness.is_finished() || worker_owner.validate_current().is_err() {
                    return Err(TrialError::Refused(RefusalReason::SessionUnproven));
                }
                blocking_trial(move || {
                    let input_owner = Arc::clone(&worker_owner);
                    let input_liveness = worker_liveness.clone();
                    let input_admission: Arc<dyn Fn() -> bool + Send + Sync> =
                        Arc::new(move || {
                            !input_liveness.is_finished() && input_owner.validate_current().is_ok()
                        });
                    let mut live =
                        Live::new_with_admission(expected.clone(), Some(input_admission))
                            .map_err(TrialError::Refused)?;
                    let owns = || {
                        !worker_liveness.is_finished() && worker_owner.validate_current().is_ok()
                    };
                    let mut bound = SessionBound {
                        device: &mut live,
                        session: &worker_session,
                        admission: Some(&owns),
                    };
                    run(&mut bound, &expected, transport)
                })
                .await
            }
        };
        let transaction = with_owned_session_idle_settings_receipt(&wda, admit, action).await;
        let (trial, close) = match transaction {
            Ok((session, outcome)) => {
                let trial = idle_result(outcome);
                (trial, Some(close_owned_session(&wda, &session).await))
            }
            Err(error) => (Err(error.message()), None),
        };
        let alive = !runner.is_finished();
        let stopped = runner.abort_and_join().await;
        owned_result(trial, close, alive, stopped.is_ok())
    });
    result?;
    println!(
        "touch_trial_owned_session=confirmed transport={}",
        transport.name()
    );
    Ok(())
}

fn owned_result(
    trial: Result<(), &'static str>,
    close: Option<SessionClose>,
    runner_alive: bool,
    runner_joined: bool,
) -> Result<(), &'static str> {
    if !runner_alive
        || !runner_joined
        || close.is_some_and(|close| close != SessionClose::Acknowledged)
    {
        Err("unknown")
    } else {
        trial
    }
}

fn settings_command(args: &[String]) -> Result<(), &'static str> {
    let (expected, transport) = options(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "refused reason=settings_unproven")?;
    runtime.block_on(async {
        let usb = PinnedUsb::new(expected.clone());
        usb.provider()
            .await
            .map_err(|_| "refused reason=identity_unproven")?;
        let wda = UsbWda::new(usb);
        let action = |worker_session: String| async move {
            blocking_trial(move || {
                // Both runtimes are constructed and dropped on the blocking thread.
                let mut live = Live::new(expected.clone()).map_err(TrialError::Refused)?;
                let mut bound = SessionBound {
                    device: &mut live,
                    session: &worker_session,
                    admission: None,
                };
                run(&mut bound, &expected, transport)
            })
            .await
        };
        let response = wda
            .request("GET", "/status", b"", Duration::from_secs(10))
            .await
            .map_err(|_| "refused reason=session_unproven")?;
        let session =
            session_from_response(response).map_err(|_| "refused reason=session_unproven")?;
        let outcome =
            with_temporary_idle_settings(&wda, &session, || action(session.clone())).await;
        idle_result(outcome)
    })?;
    println!("touch_trial_idle=confirmed transport={}", transport.name());
    Ok(())
}

async fn blocking_trial(
    work: impl FnOnce() -> Result<(), TrialError> + Send + 'static,
) -> Result<(), TrialError> {
    // Always join before the transaction restores settings; never detach or retry.
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| TrialError::Unknown)?
}

fn idle_result(outcome: IdleSettingsOutcome<(), TrialError>) -> Result<(), &'static str> {
    match (outcome.action, outcome.restoration) {
        (IdleAction::Completed(result), Restoration::Verified) => {
            result.map_err(TrialError::message)
        }
        (IdleAction::NotRun(_), Restoration::NotNeeded | Restoration::Verified) => {
            Err("refused reason=settings_unproven")
        }
        _ => Err("unknown"),
    }
}

struct Snapshot {
    observation: RawObservation,
    session: String,
    native: LockState,
    fresh_until: Instant,
}

trait TrialDevice {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason>;
    fn now_ms(&self) -> Result<u64, ()>;
    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError>;
}

struct SessionBound<'a, D> {
    device: &'a mut D,
    session: &'a str,
    admission: Option<&'a dyn Fn() -> bool>,
}

impl<D: TrialDevice> TrialDevice for SessionBound<'_, D> {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
        if self.admission.is_some_and(|admit| !admit()) {
            return Err(RefusalReason::SessionUnproven);
        }
        let snapshot = self.device.capture()?;
        if snapshot.session != self.session {
            return Err(RefusalReason::SessionUnproven);
        }
        Ok(snapshot)
    }

    fn now_ms(&self) -> Result<u64, ()> {
        self.device.now_ms()
    }

    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError> {
        if self.admission.is_some_and(|admit| !admit()) {
            return Err(CoordinateTapError::ReceiptUnproven);
        }
        self.device.tap(transport, at, fresh_until)
    }
}

fn run(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
) -> Result<(), TrialError> {
    let before = device.capture().map_err(TrialError::Refused)?;
    let first = validate(&before, expected, device.now_ms(), false).map_err(TrialError::Refused)?;
    // Reobserve every precondition; only stable accessibility geometry authorizes input.
    let current = device.capture().map_err(TrialError::Refused)?;
    let second =
        validate(&current, expected, device.now_ms(), false).map_err(TrialError::Refused)?;
    if current.session != before.session {
        return Err(TrialError::Refused(RefusalReason::SessionUnproven));
    }
    if current.observation.window.started_unix_ms < before.observation.window.finished_unix_ms {
        return Err(TrialError::Refused(RefusalReason::FreshnessUnproven));
    }
    if first != second {
        return Err(TrialError::Refused(RefusalReason::GeometryMismatch));
    }
    let input_started = device
        .now_ms()
        .map_err(|_| TrialError::Refused(RefusalReason::FreshnessUnproven))?;
    let receipt = device.tap(transport, second.center(), current.fresh_until);
    // Even unknown delivery receives one read-only postcheck, never another input.
    let after = device.capture();
    // Keep the receipt category even when the sole postcapture also fails.
    receipt.map_err(|error| {
        transport.unknown(match error {
            CoordinateTapError::Http404 => CoordinateUnknown::Http404,
            CoordinateTapError::ReceiptUnproven => CoordinateUnknown::ReceiptUnproven,
        })
    })?;
    let postcondition = transport.unknown(CoordinateUnknown::PostconditionUnproven);
    let after = after.map_err(|_| postcondition)?;
    validate(&after, expected, device.now_ms(), true).map_err(|_| postcondition)?;
    if after.session != current.session || after.observation.window.started_unix_ms < input_started
    {
        return Err(postcondition);
    }
    Ok(())
}

fn validate(
    snapshot: &Snapshot,
    expected: &HardwareId,
    now: Result<u64, ()>,
    after: bool,
) -> Result<Rect, RefusalReason> {
    let raw = &snapshot.observation;
    if Instant::now() >= snapshot.fresh_until {
        return Err(RefusalReason::FreshnessUnproven);
    }
    if raw.connected_usb != *expected || raw.wda_target != *expected {
        return Err(RefusalReason::IdentityUnproven);
    }
    if !valid_session(&snapshot.session) {
        return Err(RefusalReason::SessionUnproven);
    }
    require_native_unlocked(snapshot.native)?;
    match raw.state.lock {
        LockState::Locked => return Err(RefusalReason::WdaLocked),
        LockState::Uncertain => return Err(RefusalReason::WdaLockUnproven),
        LockState::Unlocked => {}
    }
    if !raw.state.wda_ready {
        return Err(RefusalReason::SessionUnproven);
    }
    if raw.state.foreground_app.as_deref() != Some(CLOCK) {
        return Err(RefusalReason::ForegroundNotClock);
    }
    if raw
        .window
        .duration_ms()
        .map_err(|_| RefusalReason::FreshnessUnproven)?
        > MAX_CAPTURE_MS
        || now
            .map_err(|_| RefusalReason::FreshnessUnproven)?
            .checked_sub(raw.window.finished_unix_ms)
            .ok_or(RefusalReason::FreshnessUnproven)?
            > MAX_AGE_MS
    {
        return Err(RefusalReason::FreshnessUnproven);
    }
    let png = &raw.screenshot.bytes;
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || &png[12..16] != b"IHDR" {
        return Err(RefusalReason::CaptureFailed);
    }
    let width = u32::from_be_bytes(
        png[16..20]
            .try_into()
            .map_err(|_| RefusalReason::CaptureFailed)?,
    );
    let height = u32::from_be_bytes(
        png[20..24]
            .try_into()
            .map_err(|_| RefusalReason::CaptureFailed)?,
    );
    let evidence = parse(&raw.accessibility_tree)?;
    // Coordinates come from accessibility points; image pixels only check screen shape.
    if width == 0
        || height == 0
        || (f64::from(width) / f64::from(height) - evidence.screen.w / evidence.screen.h).abs()
            > 0.02
    {
        return Err(RefusalReason::GeometryMismatch);
    }
    let explicit_selected = evidence.stopwatch.selected_present || evidence.world.selected_present;
    if !evidence
        .stopwatch
        .selection_matches(after, explicit_selected)
    {
        return Err(RefusalReason::StopwatchAttributes);
    }
    if !evidence.world.selection_matches(!after, explicit_selected) {
        return Err(RefusalReason::WorldClockAttributes);
    }
    if evidence.world_clock_navigation != u8::from(!after) {
        return Err(RefusalReason::WorldClockAttributes);
    }
    let start_matches = if after {
        evidence.start_ready
    } else {
        evidence.start_hidden_or_absent
    };
    if !start_matches {
        return Err(RefusalReason::StopwatchAttributes);
    }
    Ok(evidence.stopwatch.rect)
}

fn require_native_unlocked(state: LockState) -> Result<(), RefusalReason> {
    match state {
        LockState::Locked => Err(RefusalReason::NativeLocked),
        LockState::Uncertain => Err(RefusalReason::NativeLockUnproven),
        LockState::Unlocked => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn read(attrs: &BTreeMap<String, String>) -> Result<Self, ()> {
        let number = |name| attrs.get(name).ok_or(())?.parse::<f64>().map_err(|_| ());
        let rect = Self {
            x: number("x")?,
            y: number("y")?,
            w: number("width")?,
            h: number("height")?,
        };
        if [rect.x, rect.y, rect.w, rect.h]
            .iter()
            .any(|n| !n.is_finite() || !(0.0..=10_000.0).contains(n))
            || rect.w <= 0.0
            || rect.h <= 0.0
        {
            return Err(());
        }
        Ok(rect)
    }

    fn contains(self, other: Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.w <= self.x + self.w
            && other.y + other.h <= self.y + self.h
    }

    fn center(self) -> Point {
        Point {
            x: self.x + self.w / 2.0,
            y: self.y + self.h / 2.0,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TabValue {
    Absent,
    Active,
    Other,
}

struct Tab {
    rect: Rect,
    visible: Option<bool>,
    enabled: Option<bool>,
    selected: Option<bool>,
    selected_present: bool,
    value: TabValue,
}

impl Tab {
    fn read(attrs: &BTreeMap<String, String>) -> Result<Self, ()> {
        let flag = |key| match attrs.get(key).map(String::as_str) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        Ok(Self {
            rect: Rect::read(attrs)?,
            visible: flag("visible"),
            enabled: flag("enabled"),
            selected: flag("selected"),
            selected_present: attrs.contains_key("selected"),
            value: match attrs.get("value").map(String::as_str) {
                None => TabValue::Absent,
                Some("1") => TabValue::Active,
                _ => TabValue::Other,
            },
        })
    }

    fn eligible(&self) -> bool {
        self.visible == Some(true) && self.enabled == Some(true)
    }

    fn selection_matches(&self, active: bool, explicit_selected: bool) -> bool {
        if explicit_selected {
            self.selected == Some(active)
                && match self.value {
                    TabValue::Absent => true,
                    TabValue::Active => active,
                    TabValue::Other => false,
                }
        } else {
            !self.selected_present
                && self.value
                    == if active {
                        TabValue::Active
                    } else {
                        TabValue::Absent
                    }
        }
    }
}

struct Evidence {
    screen: Rect,
    stopwatch: Tab,
    world: Tab,
    world_clock_navigation: u8,
    start_hidden_or_absent: bool,
    start_ready: bool,
}

fn parse(xml: &str) -> Result<Evidence, RefusalReason> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(RefusalReason::CaptureFailed);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut screen = None;
    let mut tabbar = None;
    let mut stopwatch = None;
    let mut world = None;
    let mut world_clock_navigation = 0u8;
    let mut start_hidden_or_absent = true;
    let mut start_ready = false;
    let mut start_seen = false;
    loop {
        let event = reader
            .read_event()
            .map_err(|_| RefusalReason::CaptureFailed)?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                let kind = String::from_utf8(node.name().as_ref().to_vec())
                    .map_err(|_| RefusalReason::CaptureFailed)?;
                if stack.len() > 128 {
                    return Err(RefusalReason::CaptureFailed);
                }
                if matches!(
                    kind.as_str(),
                    "XCUIElementTypeAlert"
                        | "XCUIElementTypeSheet"
                        | "XCUIElementTypeSecureTextField"
                ) {
                    return Err(RefusalReason::WdaLockUnproven);
                }
                let mut attrs = BTreeMap::new();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| RefusalReason::CaptureFailed)?;
                    let key = String::from_utf8(attr.key.as_ref().to_vec())
                        .map_err(|_| RefusalReason::CaptureFailed)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| RefusalReason::CaptureFailed)?;
                    if [
                        "proud-lock",
                        "passcode-view",
                        "call-to-action-label",
                        "Enter Passcode",
                    ]
                    .iter()
                    .any(|marker| value.contains(marker))
                    {
                        return Err(RefusalReason::WdaLocked);
                    }
                    attrs.insert(key, value.into_owned());
                }
                if kind == "XCUIElementTypeApplication" {
                    if screen.is_some() {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    let rect = Rect::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?;
                    if rect.x != 0.0 || rect.y != 0.0 {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    screen = Some(rect);
                }
                if kind == "XCUIElementTypeTabBar" {
                    if tabbar.is_some() || !stack.iter().any(|s| s == "XCUIElementTypeApplication")
                    {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    tabbar = Some(Rect::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?);
                }
                let name = attrs.get("name").map(String::as_str);
                let label = attrs.get("label").map(String::as_str);
                if kind == "XCUIElementTypeNavigationBar"
                    && (name == Some("World Clock") || label == Some("World Clock"))
                    && attrs.get("visible").map(String::as_str) == Some("true")
                {
                    if !stack.iter().any(|s| s == "XCUIElementTypeApplication") {
                        return Err(RefusalReason::WorldClockAttributes);
                    }
                    if !stack.iter().any(|s| s == "XCUIElementTypeTabBar") {
                        world_clock_navigation = world_clock_navigation.saturating_add(1);
                    }
                }
                if matches!(name, Some("Stopwatch" | "World Clock"))
                    && kind == "XCUIElementTypeButton"
                {
                    if !stack.iter().any(|s| s == "XCUIElementTypeTabBar") {
                        return Err(RefusalReason::GeometryMismatch);
                    }
                    let (slot, reason) = if name == Some("Stopwatch") {
                        (&mut stopwatch, RefusalReason::StopwatchAttributes)
                    } else {
                        (&mut world, RefusalReason::WorldClockAttributes)
                    };
                    if slot.is_some() {
                        return Err(reason);
                    }
                    *slot = Some(Tab::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?);
                }
                if (name == Some("Start") || label == Some("Start"))
                    && kind == "XCUIElementTypeButton"
                    && !stack.iter().any(|s| s == "XCUIElementTypeTabBar")
                {
                    if start_seen || !stack.iter().any(|s| s == "XCUIElementTypeApplication") {
                        return Err(RefusalReason::StopwatchAttributes);
                    }
                    start_seen = true;
                    let button = Tab::read(&attrs).map_err(|_| RefusalReason::GeometryMismatch)?;
                    start_hidden_or_absent = button.visible == Some(false);
                    start_ready = button.visible == Some(true)
                        && button.enabled == Some(true)
                        && screen
                            .ok_or(RefusalReason::GeometryMismatch)?
                            .contains(button.rect);
                }
                if is_start {
                    stack.push(kind);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(RefusalReason::CaptureFailed)?;
            }
            Event::Eof => break,
            Event::DocType(_) => return Err(RefusalReason::CaptureFailed),
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(RefusalReason::CaptureFailed);
    }
    let evidence = Evidence {
        screen: screen.ok_or(RefusalReason::GeometryMismatch)?,
        stopwatch: stopwatch.ok_or(RefusalReason::StopwatchAttributes)?,
        world: world.ok_or(RefusalReason::WorldClockAttributes)?,
        world_clock_navigation,
        start_hidden_or_absent,
        start_ready,
    };
    let bar = tabbar.ok_or(RefusalReason::GeometryMismatch)?;
    if !evidence.stopwatch.eligible() {
        return Err(RefusalReason::StopwatchAttributes);
    }
    if !evidence.world.eligible() {
        return Err(RefusalReason::WorldClockAttributes);
    }
    if !evidence.screen.contains(bar)
        || !bar.contains(evidence.stopwatch.rect)
        || !bar.contains(evidence.world.rect)
        || evidence.stopwatch.rect.overlaps(evidence.world.rect)
    {
        return Err(RefusalReason::GeometryMismatch);
    }
    Ok(evidence)
}

struct Live {
    expected: HardwareId,
    adapter: WdaAdapter,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
}

impl Live {
    fn new(expected: HardwareId) -> Result<Self, RefusalReason> {
        Self::new_with_admission(expected, None)
    }

    fn new_with_admission(
        expected: HardwareId,
        input_admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self, RefusalReason> {
        Ok(Self {
            adapter: WdaAdapter::new(expected.clone()).map_err(|_| RefusalReason::CaptureFailed)?,
            wda: UsbWda::new(PinnedUsb::new(expected.clone()))
                .with_write_admission(input_admission),
            expected,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| RefusalReason::CaptureFailed)?,
            actions: WdaActions::default(),
        })
    }

    fn status(&self) -> Result<String, RefusalReason> {
        let response = self
            .runtime
            .block_on(
                self.wda
                    .request("GET", "/status", b"", Duration::from_secs(10)),
            )
            .map_err(|_| RefusalReason::SessionUnproven)?;
        session_from_response(response)
    }
}

fn session_from_response(response: HttpResponse) -> Result<String, RefusalReason> {
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| RefusalReason::SessionUnproven)?;
    let session = json["sessionId"]
        .as_str()
        .filter(|id| valid_session(id))
        .ok_or(RefusalReason::SessionUnproven)?;
    if response.status != 200
        || json["value"]["ready"] != true
        || json["value"].get("error").is_some()
    {
        return Err(RefusalReason::SessionUnproven);
    }
    Ok(session.to_owned())
}

fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl TrialDevice for Live {
    fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
        let started = self
            .now_ms()
            .map_err(|_| RefusalReason::FreshnessUnproven)?;
        self.adapter
            .identities()
            .map_err(|_| RefusalReason::IdentityUnproven)?;
        let session = self.status()?;
        require_native_unlocked(native_lock::query(&self.expected).state)?;
        let (mut observation, captured_at) = self
            .adapter
            .observe_with_attributes()
            .map_err(|_| RefusalReason::CaptureFailed)?;
        let fresh_until = captured_at
            .checked_add(Duration::from_millis(MAX_AGE_MS))
            .ok_or(RefusalReason::FreshnessUnproven)?;
        self.runtime
            .block_on(self.actions.prepare_existing(&self.wda))
            .map_err(|_| RefusalReason::SessionUnproven)?;
        if !self.actions.prepared_session_matches(&session) || self.status()? != session {
            return Err(RefusalReason::SessionUnproven);
        }
        let native = native_lock::query(&self.expected).state;
        self.adapter
            .identities()
            .map_err(|_| RefusalReason::IdentityUnproven)?;
        // Include all identity/session/native checks in the bounded capture window.
        observation.window.started_unix_ms = started;
        Ok(Snapshot {
            observation,
            session,
            native,
            fresh_until,
        })
    }

    fn now_ms(&self) -> Result<u64, ()> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_millis() as u64)
            .map_err(|_| ())
    }

    fn tap(
        &mut self,
        transport: Transport,
        at: Point,
        fresh_until: Instant,
    ) -> Result<(), CoordinateTapError> {
        let wda = self.wda.with_write_deadline(fresh_until);
        let receipt = match transport {
            Transport::W3c => self
                .runtime
                .block_on(self.actions.execute_existing(&wda, &Action::Tap { at }))
                .map_err(|_| CoordinateTapError::ReceiptUnproven),
            Transport::Coordinate => self
                .runtime
                .block_on(self.actions.tap_existing_diagnostic(&wda, at)),
        }?;
        if receipt.accepted == Some(true) {
            Ok(())
        } else {
            Err(CoordinateTapError::ReceiptUnproven)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use std::collections::VecDeque;

    type RefusalMutation = (RefusalReason, fn(&mut Snapshot));

    const REFUSAL_OUTPUTS: &[(RefusalReason, &str)] = &[
        (
            RefusalReason::NativeLocked,
            "touch_trial=refused reason=native_locked",
        ),
        (
            RefusalReason::NativeLockUnproven,
            "touch_trial=refused reason=native_lock_unproven",
        ),
        (
            RefusalReason::WdaLocked,
            "touch_trial=refused reason=wda_locked",
        ),
        (
            RefusalReason::WdaLockUnproven,
            "touch_trial=refused reason=wda_lock_unproven",
        ),
        (
            RefusalReason::ForegroundNotClock,
            "touch_trial=refused reason=foreground_not_clock",
        ),
        (
            RefusalReason::SessionUnproven,
            "touch_trial=refused reason=session_unproven",
        ),
        (
            RefusalReason::IdentityUnproven,
            "touch_trial=refused reason=identity_unproven",
        ),
        (
            RefusalReason::FreshnessUnproven,
            "touch_trial=refused reason=freshness_unproven",
        ),
        (
            RefusalReason::CaptureFailed,
            "touch_trial=refused reason=capture_failed",
        ),
        (
            RefusalReason::WorldClockAttributes,
            "touch_trial=refused reason=world_clock_attributes",
        ),
        (
            RefusalReason::StopwatchAttributes,
            "touch_trial=refused reason=stopwatch_attributes",
        ),
        (
            RefusalReason::GeometryMismatch,
            "touch_trial=refused reason=geometry_mismatch",
        ),
    ];

    fn tree(after: bool) -> String {
        format!(
            r#"<AppiumAUT><XCUIElementTypeApplication name="Clock" x="0" y="0" width="400" height="800">
          <XCUIElementTypeNavigationBar name="World Clock" visible="{before}"/>
          <XCUIElementTypeButton name="Start" visible="{after}" enabled="true" x="300" y="500" width="60" height="60"/>
          <XCUIElementTypeTabBar x="0" y="700" width="400" height="100">
            <XCUIElementTypeButton name="World Clock" visible="true" enabled="true" selected="{before}" x="0" y="700" width="100" height="80"/>
            <XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="{after}" x="200" y="700" width="100" height="80"/>
          </XCUIElementTypeTabBar>
        </XCUIElementTypeApplication></AppiumAUT>"#,
            before = !after
        )
    }

    const WORLD_NAV: &str = r#"<XCUIElementTypeNavigationBar name="World Clock" visible="true"/>"#;
    const HIDDEN_START: &str = r#"<XCUIElementTypeButton name="Start" visible="false" enabled="true" x="300" y="500" width="60" height="60"/>"#;
    const READY_START: &str = r#"<XCUIElementTypeButton name="Start" visible="true" enabled="true" x="300" y="500" width="60" height="60"/>"#;

    fn value_schema(xml: &str) -> String {
        xml.replace(" selected=\"true\"", " value=\"1\"")
            .replace(" selected=\"false\"", "")
    }

    fn snapshot(after: bool, tick: u64) -> Snapshot {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        Snapshot {
            observation: RawObservation {
                connected_usb: HardwareId::new("fixture-device").unwrap(),
                wda_target: HardwareId::new("fixture-device").unwrap(),
                window: ObservationWindow {
                    started_unix_ms: tick,
                    finished_unix_ms: tick + 1,
                },
                state: DeviceState {
                    lock: LockState::Unlocked,
                    wda_ready: true,
                    foreground_app: Some(CLOCK.into()),
                },
                accessibility_tree: tree(after),
                screenshot: ScreenImage {
                    format: ImageFormat::Png,
                    bytes: png,
                },
            },
            session: "fixture-session".into(),
            native: LockState::Unlocked,
            fresh_until: Instant::now() + Duration::from_millis(MAX_AGE_MS),
        }
    }

    struct Fixture {
        captures: VecDeque<Result<Snapshot, RefusalReason>>,
        taps: Vec<(Transport, Point, Instant)>,
        receipt: Result<(), CoordinateTapError>,
        now: u64,
        age: u64,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                captures: [
                    Ok(snapshot(false, 100)),
                    Ok(snapshot(false, 200)),
                    Ok(snapshot(true, 300)),
                ]
                .into(),
                taps: Vec::new(),
                receipt: Ok(()),
                now: 0,
                age: 10,
            }
        }
        fn run(&mut self, transport: Transport) -> Result<(), TrialError> {
            run(self, &HardwareId::new("fixture-device").unwrap(), transport)
        }
    }

    impl TrialDevice for Fixture {
        fn capture(&mut self) -> Result<Snapshot, RefusalReason> {
            let snapshot = self
                .captures
                .pop_front()
                .expect("unexpected capture or retry");
            if let Ok(snapshot) = &snapshot {
                self.now = snapshot.observation.window.finished_unix_ms + self.age;
            }
            snapshot
        }
        fn now_ms(&self) -> Result<u64, ()> {
            Ok(self.now)
        }
        fn tap(
            &mut self,
            transport: Transport,
            at: Point,
            fresh_until: Instant,
        ) -> Result<(), CoordinateTapError> {
            assert!(fresh_until > Instant::now());
            assert!(self.taps.is_empty(), "second input or transport fallback");
            self.taps.push((transport, at, fresh_until));
            self.receipt
        }
    }

    fn run_bound(
        fixture: &mut Fixture,
        transport: Transport,
        session: &str,
    ) -> Result<(), TrialError> {
        let mut bound = SessionBound {
            device: fixture,
            session,
            admission: None,
        };
        run(
            &mut bound,
            &HardwareId::new("fixture-device").unwrap(),
            transport,
        )
    }

    #[test]
    fn idle_trial_binds_both_pre_captures_to_the_settings_session() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            // Both Clock captures agree with each other but not the transaction.
            let mut wrong = Fixture::new();
            assert_eq!(
                run_bound(&mut wrong, transport, "different-valid-session"),
                Err(TrialError::Refused(RefusalReason::SessionUnproven))
            );
            assert!(wrong.taps.is_empty());
            assert_eq!(wrong.captures.len(), 2);

            let mut drift = Fixture::new();
            drift.captures[1].as_mut().unwrap().session = "replacement-session".into();
            assert_eq!(
                run_bound(&mut drift, transport, "fixture-session"),
                Err(TrialError::Refused(RefusalReason::SessionUnproven))
            );
            assert!(drift.taps.is_empty());
            assert_eq!(drift.captures.len(), 1);
        }
    }

    #[test]
    fn idle_trial_post_session_mismatch_is_unknown_without_replay() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for changed in [false, true] {
                let mut fixture = Fixture::new();
                if changed {
                    fixture.captures[2].as_mut().unwrap().session = "replacement-session".into();
                }
                assert_eq!(
                    run_bound(&mut fixture, transport, "fixture-session"),
                    if changed {
                        Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                    } else {
                        Ok(())
                    }
                );
                assert_eq!(fixture.taps.len(), 1);
                assert!(fixture.captures.is_empty());
            }
        }
    }

    #[test]
    fn owned_trial_refuses_lost_admission_before_input() {
        use std::cell::Cell;
        for fail_on in [1, 2, 3] {
            let checks = Cell::new(0);
            let admit = || {
                checks.set(checks.get() + 1);
                checks.get() < fail_on
            };
            let mut fixture = Fixture::new();
            let mut bound = SessionBound {
                device: &mut fixture,
                session: "fixture-session",
                admission: Some(&admit),
            };
            assert!(
                run(
                    &mut bound,
                    &HardwareId::new("fixture-device").unwrap(),
                    Transport::W3c
                )
                .is_err()
            );
            assert!(fixture.taps.is_empty());
        }
    }

    #[test]
    fn runner_exit_after_clock_captures_prevents_tap() {
        use std::cell::Cell;
        let runner_alive = Cell::new(true);
        let checks = Cell::new(0);
        let admit = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                // Both captures are allowed; the runner exits before the tap guard.
                runner_alive.set(false);
                true
            } else {
                runner_alive.get()
            }
        };
        let mut fixture = Fixture::new();
        let mut bound = SessionBound {
            device: &mut fixture,
            session: "fixture-session",
            admission: Some(&admit),
        };
        assert_eq!(
            run(
                &mut bound,
                &HardwareId::new("fixture-device").unwrap(),
                Transport::W3c,
            ),
            Err(TrialError::Unknown)
        );
        assert_eq!(checks.get(), 4);
        assert!(fixture.taps.is_empty());
        assert_eq!(fixture.captures.len(), 1);
    }

    #[test]
    fn owned_trial_never_confirms_uncertain_cleanup() {
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), true, true),
            Ok(())
        );
        for close in [SessionClose::Refused, SessionClose::Unknown] {
            assert_eq!(
                owned_result(Ok(()), Some(close), true, true),
                Err("unknown")
            );
        }
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), false, true),
            Err("unknown")
        );
        assert_eq!(
            owned_result(Ok(()), Some(SessionClose::Acknowledged), true, false),
            Err("unknown")
        );
        assert_eq!(
            owned_result(Err("refused reason=session_admission"), None, true, true),
            Err("refused reason=session_admission")
        );
    }

    #[test]
    fn idle_bridge_requires_verified_restore_and_preserves_fixed_outcomes() {
        use handkite_host::wda_settings::SettingsError;
        for restoration in [
            Restoration::NotNeeded,
            Restoration::Verified,
            Restoration::Unproven(SettingsError::WriteUnproven),
        ] {
            for result in [
                Ok(()),
                Err(TrialError::Unknown),
                Err(TrialError::CoordinateUnknown(CoordinateUnknown::Http404)),
                Err(TrialError::CoordinateUnknown(
                    CoordinateUnknown::ReceiptUnproven,
                )),
                Err(TrialError::CoordinateUnknown(
                    CoordinateUnknown::PostconditionUnproven,
                )),
                Err(TrialError::Refused(RefusalReason::SessionUnproven)),
            ] {
                let expected = if restoration == Restoration::Verified {
                    result.map_err(TrialError::message)
                } else {
                    Err("unknown")
                };
                assert_eq!(
                    idle_result(IdleSettingsOutcome {
                        action: IdleAction::Completed(result),
                        restoration,
                    }),
                    expected
                );
            }
            assert_eq!(
                idle_result(IdleSettingsOutcome {
                    action: IdleAction::NotRun(SettingsError::SessionUnproven),
                    restoration,
                }),
                if matches!(restoration, Restoration::Unproven(_)) {
                    Err("unknown")
                } else {
                    Err("refused reason=settings_unproven")
                }
            );
        }
    }

    #[tokio::test]
    async fn blocking_bridge_awaits_worker_runtime_and_maps_join_panic_to_unknown() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        assert_eq!(
            blocking_trial(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    worker_calls.fetch_add(1, Ordering::SeqCst);
                });
                Ok(())
            })
            .await,
            Ok(())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            blocking_trial(|| panic!("fixture worker panic")).await,
            Err(TrialError::Unknown)
        );
    }

    #[test]
    fn each_transport_sends_once_at_accessibility_center_then_observes_once() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            let mut fixture = Fixture::new();
            let expires = fixture.captures[1].as_ref().unwrap().fresh_until;
            assert_eq!(fixture.run(transport), Ok(()));
            assert_eq!(
                fixture.taps,
                [(transport, Point { x: 250.0, y: 740.0 }, expires)]
            );
            assert!(fixture.captures.is_empty());
        }
    }

    #[test]
    fn both_coherent_schemas_require_the_reciprocal_content_transition() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for schema in 0..3 {
                for start_absent in [false, true] {
                    let mut fixture = Fixture::new();
                    for (index, capture) in fixture.captures.iter_mut().enumerate() {
                        let xml = &mut capture.as_mut().unwrap().observation.accessibility_tree;
                        if schema == 1 {
                            *xml = value_schema(xml);
                        }
                        if schema == 2 {
                            *xml =
                                xml.replace("selected=\"true\"", "selected=\"true\" value=\"1\"");
                        }
                        if start_absent && index < 2 {
                            *xml = xml.replace(HIDDEN_START, "");
                        }
                        *xml = xml
                            .replace(
                                "NavigationBar name=\"World Clock\"",
                                "NavigationBar label=\"World Clock\"",
                            )
                            .replace("Button name=\"Start\"", "Button label=\"Start\"");
                    }
                    assert_eq!(fixture.run(transport), Ok(()));
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn mixed_conflicting_or_unrecognized_selection_evidence_refuses_both_transports() {
        let explicit = tree(false);
        let values = value_schema(&explicit);
        let inactive = "name=\"Stopwatch\" visible=\"true\" enabled=\"true\"";
        let cases = [
            explicit.replace("selected=\"true\"", "selected=\"false\""),
            explicit.replace("selected=\"false\"", "selected=\"true\""),
            explicit.replace("selected=\"false\"", "selected=\"invalid\""),
            explicit.replace(" selected=\"false\"", ""),
            explicit.replace(" selected=\"true\"", ""),
            explicit.replace("selected=\"false\"", "selected=\"false\" value=\"1\""),
            explicit.replace("selected=\"true\"", "selected=\"true\" value=\"other\""),
            values.replace(" value=\"1\"", ""),
            values.replace(inactive, &format!("{inactive} value=\"1\"")),
            values.replace(inactive, &format!("{inactive} value=\"0\"")),
            values.replace(inactive, &format!("{inactive} selected=\"false\"")),
            values.replace("value=\"1\"", "selected=\"true\" value=\"1\""),
            values.replace("value=\"1\"", "selected=\"invalid\" value=\"1\""),
            values.replace("value=\"1\"", "value=\"0\""),
            values.replace("value=\"1\"", "value=\"true\""),
            values.replace("value=\"1\"", "value=\"false\""),
            values.replace("value=\"1\"", "value=\" 1\""),
            values.replace("value=\"1\"", "value=\"\""),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for position in 0..2 {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    fixture.captures[position]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = xml.clone();
                    assert!(matches!(
                        fixture.run(transport),
                        Err(TrialError::Refused(_))
                    ));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn before_content_rejects_other_clock_tabs_overlays_and_ambiguous_markers() {
        let before = tree(false);
        let cases = [
            before.replace(WORLD_NAV, &WORLD_NAV.replace("World Clock", "Alarm")),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("World Clock", "Timers")),
            before.replace(WORLD_NAV, ""),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("true", "false")),
            before.replace(WORLD_NAV, &WORLD_NAV.replace("NavigationBar", "StaticText")),
            before.replace(WORLD_NAV, &WORLD_NAV.repeat(2)),
            before
                .replace(WORLD_NAV, "")
                .replace("</AppiumAUT>", &format!("{WORLD_NAV}</AppiumAUT>")),
            before.replace(WORLD_NAV, "").replace(
                "</XCUIElementTypeTabBar>",
                &format!("{WORLD_NAV}</XCUIElementTypeTabBar>"),
            ),
            before.replace(HIDDEN_START, READY_START),
            before.replace(
                HIDDEN_START,
                &READY_START.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            before.replace(
                HIDDEN_START,
                &HIDDEN_START.replace(" visible=\"false\"", ""),
            ),
            before.replace(HIDDEN_START, &HIDDEN_START.repeat(2)),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
            ),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeSheet/></XCUIElementTypeApplication>",
            ),
            before.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeSecureTextField/></XCUIElementTypeApplication>",
            ),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for values in [false, true] {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    fixture.captures[1]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = if values {
                        value_schema(xml)
                    } else {
                        xml.clone()
                    };
                    assert!(matches!(
                        fixture.run(transport),
                        Err(TrialError::Refused(_))
                    ));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn invalid_after_content_or_schema_is_unknown_after_exactly_one_tap() {
        let after = tree(true);
        let cases = [
            after.replace(&WORLD_NAV.replace("true", "false"), WORLD_NAV),
            after.replace(READY_START, ""),
            after.replace(READY_START, HIDDEN_START),
            after.replace(
                READY_START,
                &READY_START.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            after.replace(READY_START, &READY_START.repeat(2)),
            after
                .replace(READY_START, "")
                .replace("</AppiumAUT>", &format!("{READY_START}</AppiumAUT>")),
            after.replace(READY_START, "").replace(
                "</XCUIElementTypeTabBar>",
                &format!("{READY_START}</XCUIElementTypeTabBar>"),
            ),
            after.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
            ),
            after.replace("selected=\"false\"", "selected=\"true\""),
            after.replace(" selected=\"true\"", ""),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for values in [false, true] {
                for xml in &cases {
                    let mut fixture = Fixture::new();
                    if values {
                        for capture in &mut fixture.captures {
                            let source =
                                &mut capture.as_mut().unwrap().observation.accessibility_tree;
                            *source = value_schema(source);
                        }
                    }
                    fixture.captures[2]
                        .as_mut()
                        .unwrap()
                        .observation
                        .accessibility_tree = if values {
                        value_schema(xml)
                    } else {
                        xml.clone()
                    };
                    assert_eq!(
                        fixture.run(transport),
                        Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                    );
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn both_preflight_captures_reject_stale_mismatched_locked_and_unready_evidence() {
        use RefusalReason::*;
        let mutations: &[RefusalMutation] = &[
            (FreshnessUnproven, |s| s.fresh_until = Instant::now()),
            (IdentityUnproven, |s| {
                s.observation.connected_usb = HardwareId::new("other-device").unwrap()
            }),
            (IdentityUnproven, |s| {
                s.observation.wda_target = HardwareId::new("other-device").unwrap()
            }),
            (NativeLocked, |s| s.native = LockState::Locked),
            (NativeLockUnproven, |s| s.native = LockState::Uncertain),
            (WdaLocked, |s| s.observation.state.lock = LockState::Locked),
            (WdaLockUnproven, |s| {
                s.observation.state.lock = LockState::Uncertain
            }),
            (SessionUnproven, |s| s.observation.state.wda_ready = false),
            (ForegroundNotClock, |s| {
                s.observation.state.foreground_app = Some("com.apple.springboard".into())
            }),
            (SessionUnproven, |s| s.session = "../invalid".into()),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 0
            }),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 20_000
            }),
            (CaptureFailed, |s| s.observation.screenshot.bytes.clear()),
            (StopwatchAttributes, |s| {
                s.observation.accessibility_tree = tree(true)
            }),
            (WorldClockAttributes, |s| {
                s.observation.accessibility_tree =
                    tree(false).replace("selected=\"true\"", "selected=\"false\"")
            }),
            (GeometryMismatch, |s| {
                s.observation.accessibility_tree = tree(false).replace("x=\"200\"", "x=\"NaN\"")
            }),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for position in 0..2 {
                for (reason, mutate) in mutations {
                    let mut fixture = Fixture::new();
                    mutate(fixture.captures[position].as_mut().unwrap());
                    assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                    assert!(fixture.taps.is_empty());
                }
            }
        }
        let mut fixture = Fixture::new();
        fixture.age = 4000;
        assert_eq!(
            fixture.run(Transport::Coordinate),
            Err(TrialError::Refused(FreshnessUnproven))
        );
        assert!(fixture.taps.is_empty());
    }

    #[test]
    fn revalidation_refuses_changed_session_geometry_or_reused_capture() {
        let mutations: &[RefusalMutation] = &[
            (RefusalReason::SessionUnproven, |s| {
                s.session = "replacement-session".into()
            }),
            (RefusalReason::GeometryMismatch, |s| {
                s.observation.accessibility_tree = tree(false).replace("x=\"200\"", "x=\"210\"")
            }),
            (RefusalReason::FreshnessUnproven, |s| {
                s.observation.window.started_unix_ms = 100
            }),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (reason, mutate) in mutations {
                let mut fixture = Fixture::new();
                mutate(fixture.captures[1].as_mut().unwrap());
                assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                assert!(fixture.taps.is_empty());
            }
        }
    }

    #[test]
    fn ambiguous_hidden_disabled_missing_attributes_and_unsafe_trees_refuse_input() {
        let original = tree(false);
        use RefusalReason::*;
        let duplicate = r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="false" x="200" y="700" width="100" height="80"/>"#;
        let cases = [
            (
                StopwatchAttributes,
                original.replace(
                    "</XCUIElementTypeTabBar>",
                    &format!("{duplicate}</XCUIElementTypeTabBar>"),
                ),
            ),
            (
                StopwatchAttributes,
                original.replace("visible=\"true\"", "visible=\"false\""),
            ),
            (
                StopwatchAttributes,
                original.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            (
                StopwatchAttributes,
                original.replace(" selected=\"false\"", ""),
            ),
            (
                StopwatchAttributes,
                original.replace(" visible=\"true\"", ""),
            ),
            (
                StopwatchAttributes,
                original.replace(" enabled=\"true\"", ""),
            ),
            (
                WorldClockAttributes,
                original.replace("name=\"World Clock\"", "name=\"Other\""),
            ),
            (GeometryMismatch, original.replace("x=\"200\"", "x=\"NaN\"")),
            (GeometryMismatch, original.replace("x=\"200\"", "x=\"399\"")),
            (
                WdaLockUnproven,
                original.replace("</AppiumAUT>", "<XCUIElementTypeAlert/></AppiumAUT>"),
            ),
            (
                WdaLockUnproven,
                original.replace(
                    "</AppiumAUT>",
                    "<XCUIElementTypeSecureTextField/></AppiumAUT>",
                ),
            ),
            (
                WdaLocked,
                original.replace("</AppiumAUT>", "<node name=\"proud-lock\"/></AppiumAUT>"),
            ),
            (CaptureFailed, original.replace("</AppiumAUT>", "")),
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (reason, xml) in &cases {
                let mut fixture = Fixture::new();
                fixture.captures[1]
                    .as_mut()
                    .unwrap()
                    .observation
                    .accessibility_tree = xml.clone();
                assert_eq!(fixture.run(transport), Err(TrialError::Refused(*reason)));
                assert!(fixture.taps.is_empty());
            }
        }
    }

    #[test]
    fn acknowledged_input_without_the_postcondition_remains_unknown_without_retry() {
        let mutations: &[fn(&mut Snapshot)] = &[
            |s| s.observation.accessibility_tree = tree(false),
            |s| {
                s.observation.accessibility_tree =
                    tree(true).replace("name=\"Start\"", "name=\"Other\"")
            },
            |s| s.native = LockState::Locked,
            |s| s.session = "replacement-session".into(),
            |s| s.observation.connected_usb = HardwareId::new("other-device").unwrap(),
            |s| s.observation.window.started_unix_ms = 100,
        ];
        for transport in [Transport::W3c, Transport::Coordinate] {
            for mutate in mutations {
                let mut fixture = Fixture::new();
                mutate(fixture.captures[2].as_mut().unwrap());
                assert_eq!(
                    fixture.run(transport),
                    Err(transport.unknown(CoordinateUnknown::PostconditionUnproven))
                );
                assert_eq!(fixture.taps.len(), 1);
                assert!(fixture.captures.is_empty());
            }
        }
        let mut fixture = Fixture::new();
        fixture.captures[2] = Err(RefusalReason::CaptureFailed);
        assert_eq!(
            fixture.run(Transport::Coordinate),
            Err(TrialError::CoordinateUnknown(
                CoordinateUnknown::PostconditionUnproven
            ))
        );
        assert_eq!(fixture.taps.len(), 1);
    }

    #[test]
    fn unknown_receipt_keeps_its_fixed_category_after_exactly_one_postcheck() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for (receipt, reason, output) in [
                (
                    CoordinateTapError::Http404,
                    CoordinateUnknown::Http404,
                    "touch_trial=unknown reason=http_404",
                ),
                (
                    CoordinateTapError::ReceiptUnproven,
                    CoordinateUnknown::ReceiptUnproven,
                    "touch_trial=unknown reason=receipt_unproven",
                ),
            ] {
                for post in 0..3 {
                    let mut fixture = Fixture::new();
                    fixture.receipt = Err(receipt);
                    match post {
                        0 => {}
                        1 => fixture.captures[2] = Err(RefusalReason::CaptureFailed),
                        _ => {
                            fixture.captures[2]
                                .as_mut()
                                .unwrap()
                                .observation
                                .accessibility_tree = tree(false)
                        }
                    }
                    let error = fixture.run(transport).unwrap_err();
                    assert_eq!(error, transport.unknown(reason));
                    assert_eq!(
                        format!("touch_trial={}", error.message()),
                        if transport == Transport::Coordinate {
                            output
                        } else {
                            "touch_trial=unknown"
                        }
                    );
                    assert_eq!(fixture.taps.len(), 1);
                    assert!(fixture.captures.is_empty());
                }
            }
        }
    }

    #[test]
    fn capture_failures_have_fixed_output_and_never_report_refusal_after_input() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            for &(reason, output) in REFUSAL_OUTPUTS {
                for position in 0..3 {
                    let mut fixture = Fixture::new();
                    fixture.captures[position] = Err(reason);
                    let error = fixture.run(transport).unwrap_err();
                    if position < 2 {
                        assert_eq!(error, TrialError::Refused(reason));
                        assert_eq!(format!("touch_trial={}", error.message()), output);
                        assert!(fixture.taps.is_empty());
                    } else {
                        assert_eq!(
                            error,
                            transport.unknown(CoordinateUnknown::PostconditionUnproven)
                        );
                        let expected = if transport == Transport::Coordinate {
                            "touch_trial=unknown reason=postcondition_unproven"
                        } else {
                            "touch_trial=unknown"
                        };
                        assert_eq!(format!("touch_trial={}", error.message()), expected);
                        assert_eq!(fixture.taps.len(), 1);
                    }
                    assert_eq!(fixture.captures.len(), 2 - position);
                }
            }
        }
    }

    #[test]
    fn first_failed_guard_keeps_its_original_precedence() {
        use RefusalReason::*;
        let expected = HardwareId::new("fixture-device").unwrap();
        let mut sample = snapshot(false, 100);
        sample.fresh_until = Instant::now();
        sample.observation.connected_usb = HardwareId::new("other-device").unwrap();
        sample.session.clear();
        sample.native = LockState::Locked;
        sample.observation.state.lock = LockState::Locked;
        sample.observation.state.wda_ready = false;
        sample.observation.state.foreground_app = None;
        sample.observation.window.finished_unix_ms = 0;
        let repairs: &[RefusalMutation] = &[
            (FreshnessUnproven, |s| {
                s.fresh_until = Instant::now() + Duration::from_millis(MAX_AGE_MS)
            }),
            (IdentityUnproven, |s| {
                s.observation.connected_usb = HardwareId::new("fixture-device").unwrap()
            }),
            (SessionUnproven, |s| s.session = "fixture-session".into()),
            (NativeLocked, |s| s.native = LockState::Unlocked),
            (WdaLocked, |s| {
                s.observation.state.lock = LockState::Unlocked
            }),
            (SessionUnproven, |s| s.observation.state.wda_ready = true),
            (ForegroundNotClock, |s| {
                s.observation.state.foreground_app = Some(CLOCK.into())
            }),
            (FreshnessUnproven, |s| {
                s.observation.window.finished_unix_ms = 101
            }),
        ];
        for (reason, repair) in repairs {
            assert_eq!(validate(&sample, &expected, Err(()), false), Err(*reason));
            repair(&mut sample);
        }
        assert_eq!(
            validate(&sample, &expected, Err(()), false),
            Err(FreshnessUnproven)
        );
        assert!(validate(&sample, &expected, Ok(1000), false).is_ok());
    }

    #[test]
    fn arguments_and_config_fail_before_device_initialization() {
        for args in [
            vec![],
            vec!["--transport", "w3c"],
            vec!["--config", "missing", "--transport", "fallback"],
            vec!["--config", "missing", "--transport", "w3c", "extra"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(command(&args), Err("invalid_arguments"));
            assert_eq!(idle_command(&args), Err("invalid_arguments"));
            assert_eq!(owned_command(&args), Err("invalid_arguments"));
        }
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.json");
        std::fs::write(
            &config,
            br#"{"device_key":"fixture","usb_id":"fixture-device"}"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            idle_command(&[
                "--config".into(),
                config.to_str().unwrap().into(),
                "--transport".into(),
                "w3c".into()
            ]),
            Err("configuration_invalid")
        );
        assert_eq!(
            command(&[
                "--config".into(),
                config.to_str().unwrap().into(),
                "--transport".into(),
                "w3c".into()
            ]),
            Err("configuration_invalid")
        );
    }
}
