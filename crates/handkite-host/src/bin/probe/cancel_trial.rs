use super::pin_geometry::{LockedCapture, LockedObserver, keypad_geometry};
use handkite_core::protocol::{Action, HardwareId, LockState, Point};
use handkite_host::host_config;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_action::WdaActions;
use handkite_host::wda_settings::{
    IdleAction, IdleSettingsOutcome, Restoration, validated_active_session,
    with_temporary_idle_settings,
};
use handkite_host::wda_transport::UsbWda;
use quick_xml::events::Event;
use std::path::Path;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Error {
    Capture,
    Session,
    Freshness,
    Screen,
    Geometry,
    Supervision,
    Unknown,
}

impl Error {
    fn message(self) -> &'static str {
        match self {
            Self::Capture => "refused reason=capture_unproven",
            Self::Session => "refused reason=session_unproven",
            Self::Freshness => "refused reason=freshness_unproven",
            Self::Screen => "refused reason=screen_unproven",
            Self::Geometry => "refused reason=geometry_unproven",
            Self::Supervision => "refused reason=supervision_required",
            Self::Unknown => "unknown",
        }
    }
}

fn options(args: &[String]) -> Result<(HardwareId, bool), &'static str> {
    let (path, supervised) = match args {
        [flag, path] if flag == "--config" => (path, false),
        [flag, path, supervision]
            if flag == "--config" && supervision == "--supervised-hidden-cancel" =>
        {
            (path, true)
        }
        _ => return Err("refused reason=invalid_arguments"),
    };
    let expected = host_config::load(Path::new(path))
        .map_err(|_| "refused reason=configuration_invalid")?
        .usb_id;
    Ok((expected, supervised))
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let (expected, supervised) = options(args)?;
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
        let session = validated_active_session(&wda)
            .await
            .map_err(|_| "refused reason=session_unproven")?;
        let worker_session = session.clone();
        let outcome = with_temporary_idle_settings(&wda, &session, || async move {
            blocking_trial(move || {
                // Construct and drop synchronous runtimes entirely inside this worker.
                let mut live = Live::new(expected.clone())?;
                run(&mut live, &expected, &worker_session, supervised)
            })
            .await
        })
        .await;
        idle_result(outcome)
    })?;
    println!("cancel_trial_idle=confirmed");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CompetitorKind {
    Button,
    OtherControl,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NameClass {
    Emergency,
    Delete,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AttributeFlag {
    True,
    False,
    Missing,
}

impl AttributeFlag {
    fn parse(value: &str) -> Self {
        match value {
            "true" => Self::True,
            "false" => Self::False,
            _ => Self::Missing,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::True => "true",
            Self::False => "false",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CompetitorFlags {
    enabled: AttributeFlag,
    visible: AttributeFlag,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GeometryClass {
    ProtectedCountInvalid,
    OverlapKeyOrField,
    OverlapOtherControl,
    CompetitorBoundsInvalid(CompetitorKind, NameClass, CompetitorFlags),
    Stable,
}

impl GeometryClass {
    fn invalid_competitor(
        kind: &[u8],
        name: Option<&str>,
        label: Option<&str>,
        flags: CompetitorFlags,
    ) -> Self {
        let kind = if kind == b"XCUIElementTypeButton" {
            CompetitorKind::Button
        } else {
            CompetitorKind::OtherControl
        };
        let names = [name, label];
        let name = match (
            names.contains(&Some("Emergency")),
            names.contains(&Some("Delete")),
        ) {
            (true, false) => NameClass::Emergency,
            (false, true) => NameClass::Delete,
            _ => NameClass::Other,
        };
        Self::CompetitorBoundsInvalid(kind, name, flags)
    }

    fn line(self) -> String {
        let base = match self {
            Self::ProtectedCountInvalid => {
                "cancel_geometry=observed classification=protected_count_invalid"
            }
            Self::OverlapKeyOrField => {
                "cancel_geometry=observed classification=overlap_key_or_field"
            }
            Self::OverlapOtherControl => {
                "cancel_geometry=observed classification=overlap_other_control"
            }
            Self::CompetitorBoundsInvalid(CompetitorKind::Button, NameClass::Emergency, _) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=button name_class=emergency"
            }
            Self::CompetitorBoundsInvalid(CompetitorKind::Button, NameClass::Delete, _) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=button name_class=delete"
            }
            Self::CompetitorBoundsInvalid(CompetitorKind::Button, NameClass::Other, _) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=button name_class=other"
            }
            Self::CompetitorBoundsInvalid(
                CompetitorKind::OtherControl,
                NameClass::Emergency,
                _,
            ) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=other_control name_class=emergency"
            }
            Self::CompetitorBoundsInvalid(CompetitorKind::OtherControl, NameClass::Delete, _) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=other_control name_class=delete"
            }
            Self::CompetitorBoundsInvalid(CompetitorKind::OtherControl, NameClass::Other, _) => {
                "cancel_geometry=observed classification=competitor_bounds_invalid kind=other_control name_class=other"
            }
            Self::Stable => "cancel_geometry=observed classification=stable",
        };
        match self {
            Self::CompetitorBoundsInvalid(_, _, flags) => format!(
                "{base} enabled={} visible={}",
                flags.enabled.token(),
                flags.visible.token()
            ),
            _ => base.to_owned(),
        }
    }
}

// This diagnostic interface has no settings, input, or session-creation operation.
trait ReadOnly {
    fn capture(&mut self) -> Result<LockedCapture, Error>;
    fn now(&self) -> Instant;
}

impl ReadOnly for LockedObserver {
    fn capture(&mut self) -> Result<LockedCapture, Error> {
        LockedObserver::capture(self).map_err(|_| Error::Capture)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub(super) fn geometry_command(args: &[String]) -> Result<(), &'static str> {
    let (expected, supervised) = options(args)?;
    let mut observer =
        LockedObserver::new(expected.clone()).map_err(|_| Error::Capture.message())?;
    let class = inspect_geometry(&mut observer, &expected, supervised).map_err(Error::message)?;
    println!("{}", class.line());
    Ok(())
}

fn inspect_geometry(
    device: &mut impl ReadOnly,
    expected: &HardwareId,
    supervised: bool,
) -> Result<GeometryClass, Error> {
    let before = device.capture()?;
    check_capture(&before, expected, &before.session, device.now())?;
    let first = match parse_cancel(&before, supervised) {
        Ok(evidence) => evidence,
        Err(failure) => {
            return failure
                .classification
                .map(Ok)
                .unwrap_or(Err(failure.refusal));
        }
    };
    let current = device.capture()?;
    check_capture(&current, expected, &before.session, device.now())?;
    if current.started < before.finished
        || current.started <= before.started
        || current.finished <= before.finished
    {
        return Err(Error::Freshness);
    }
    let second = match parse_cancel(&current, supervised) {
        Ok(evidence) => evidence,
        Err(failure) => {
            return failure
                .classification
                .map(Ok)
                .unwrap_or(Err(failure.refusal));
        }
    };
    if first != second {
        return Err(Error::Geometry);
    }
    Ok(GeometryClass::Stable)
}

async fn blocking_trial(
    work: impl FnOnce() -> Result<(), Error> + Send + 'static,
) -> Result<(), Error> {
    // Restoration must wait for completion, including a worker panic. Never detach.
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Unknown)?
}

fn idle_result(outcome: IdleSettingsOutcome<(), Error>) -> Result<(), &'static str> {
    match (outcome.action, outcome.restoration) {
        (IdleAction::Completed(result), Restoration::Verified) => result.map_err(Error::message),
        (IdleAction::NotRun(_), Restoration::NotNeeded | Restoration::Verified) => {
            Err("refused reason=settings_unproven")
        }
        _ => Err("unknown"),
    }
}

trait Device {
    fn capture(&mut self) -> Result<LockedCapture, Error>;
    fn now(&self) -> Instant;
    fn tap(&mut self, session: &str, at: Point, fresh_until: Instant) -> Result<(), ()>;
}

fn check_capture(
    capture: &LockedCapture,
    expected: &HardwareId,
    session: &str,
    now: Instant,
) -> Result<(), Error> {
    if capture.session != session || !capture.raw.state.wda_ready {
        return Err(Error::Session);
    }
    if capture.raw.connected_usb != *expected
        || capture.raw.wda_target != *expected
        || capture.raw.state.lock != LockState::Locked
        || capture.raw.state.foreground_app.as_deref() != Some("com.apple.springboard")
    {
        return Err(Error::Screen);
    }
    if capture.started > capture.finished
        || capture.finished > now
        || capture.started.checked_add(BUDGET) != Some(capture.fresh_until)
        || now >= capture.fresh_until
        || capture
            .raw
            .window
            .duration_ms()
            .map_err(|_| Error::Freshness)?
            > BUDGET.as_millis() as u64
    {
        return Err(Error::Freshness);
    }
    Ok(())
}

fn run(
    device: &mut impl Device,
    expected: &HardwareId,
    session: &str,
    supervised: bool,
) -> Result<(), Error> {
    let before = device.capture()?;
    check_capture(&before, expected, session, device.now())?;
    let first = cancel(&before, supervised)?;
    let current = device.capture()?;
    check_capture(&current, expected, session, device.now())?;
    let second = cancel(&current, supervised)?;
    if current.started < before.finished
        || current.started <= before.started
        || current.finished <= before.finished
    {
        return Err(Error::Freshness);
    }
    if first != second {
        return Err(Error::Geometry);
    }
    let input_started = device.now();
    if input_started >= current.fresh_until {
        return Err(Error::Freshness);
    }
    let receipt = device.tap(session, second.cancel.center(), current.fresh_until);
    // All failures after entering dispatch are unknown. Always check once, never replay.
    let captured = device.capture();
    let after = captured.map_err(|_| Error::Unknown)?;
    if receipt.is_err() {
        return Err(Error::Unknown);
    }
    check_capture(&after, expected, session, device.now()).map_err(|_| Error::Unknown)?;
    let size = super::reveal_keypad::normal_lock_size(&after.raw.accessibility_tree)
        .ok_or(Error::Unknown)?;
    let image = image_size(&after.raw.screenshot.bytes, size).map_err(|_| Error::Unknown)?;
    if after.started < input_started
        || size != (f64::from(second.screen.0), f64::from(second.screen.1))
        || image != second.image
    {
        return Err(Error::Unknown);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect([f64; 4]);

impl Rect {
    fn read(values: [Option<f64>; 4], screen: (u16, u16)) -> Result<Self, Error> {
        let [Some(x), Some(y), Some(w), Some(h)] = values else {
            return Err(Error::Geometry);
        };
        if ![x, y, w, h].iter().all(|v| v.is_finite())
            || x < 0.0
            || y < 0.0
            || w <= 0.0
            || h <= 0.0
            || x + w > f64::from(screen.0)
            || y + h > f64::from(screen.1)
        {
            return Err(Error::Geometry);
        }
        Ok(Self([x, y, w, h]))
    }

    fn center(self) -> Point {
        let [x, y, w, h] = self.0;
        Point {
            x: x + w / 2.0,
            y: y + h / 2.0,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        let [x, y, w, h] = self.0;
        let [ox, oy, ow, oh] = other.0;
        x < ox + ow && ox < x + w && y < oy + oh && oy < y + h
    }
}

#[derive(Debug, PartialEq)]
struct Evidence {
    screen: (u16, u16),
    image: (u32, u32),
    cancel: Rect,
    visible: bool,
    protected: Vec<Rect>,
    competing: Vec<Rect>,
}

struct CancelFailure {
    refusal: Error,
    classification: Option<GeometryClass>,
}

impl From<Error> for CancelFailure {
    fn from(refusal: Error) -> Self {
        Self {
            refusal,
            classification: None,
        }
    }
}

impl CancelFailure {
    fn classified(classification: GeometryClass) -> Self {
        Self {
            refusal: Error::Geometry,
            classification: Some(classification),
        }
    }
}

fn cancel(capture: &LockedCapture, supervised: bool) -> Result<Evidence, Error> {
    parse_cancel(capture, supervised).map_err(|failure| failure.refusal)
}

fn classify_rectangles(cancel: Rect, protected: &[Rect], competing: &[Rect]) -> GeometryClass {
    if protected.len() != 11 {
        GeometryClass::ProtectedCountInvalid
    } else if protected.iter().any(|rect| cancel.overlaps(*rect)) {
        GeometryClass::OverlapKeyOrField
    } else if competing.iter().any(|rect| cancel.overlaps(*rect)) {
        GeometryClass::OverlapOtherControl
    } else {
        GeometryClass::Stable
    }
}

fn parse_cancel(capture: &LockedCapture, supervised: bool) -> Result<Evidence, CancelFailure> {
    let xml = &capture.raw.accessibility_tree;
    let png = &capture.raw.screenshot.bytes;
    // Includes bounded strict XML, exact zero-of-six progress, field/key uniqueness,
    // no alert/trust, in-Application ancestry and keypad layout, and zero screen origin.
    let screen = keypad_geometry(xml, png).map_err(|_| Error::Screen)?;
    let image = image_size(png, (f64::from(screen.0), f64::from(screen.1)))?;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut candidate = None;
    let mut protected = Vec::new();
    let mut competing = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| Error::Screen)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                let (mut name, mut label, mut visible, mut enabled) = (None, None, None, None);
                let mut flags = CompetitorFlags {
                    enabled: AttributeFlag::Missing,
                    visible: AttributeFlag::Missing,
                };
                let mut rect = [None; 4];
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| Error::Screen)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| Error::Screen)?;
                    match attr.key.as_ref() {
                        b"name" => name = Some(value.into_owned()),
                        b"label" => label = Some(value.into_owned()),
                        b"visible" => {
                            flags.visible = AttributeFlag::parse(&value);
                            visible = match value.as_ref() {
                                "true" => Some(true),
                                "false" => Some(false),
                                _ => None,
                            }
                        }
                        b"enabled" => {
                            flags.enabled = AttributeFlag::parse(&value);
                            enabled = Some(value == "true");
                        }
                        b"x" => rect[0] = value.parse().ok(),
                        b"y" => rect[1] = value.parse().ok(),
                        b"width" => rect[2] = value.parse().ok(),
                        b"height" => rect[3] = value.parse().ok(),
                        _ => {}
                    }
                }
                let kind = node.name();
                let inside = kind.as_ref() == b"XCUIElementTypeApplication"
                    || stack.last().copied().unwrap_or(false);
                if kind.as_ref() == b"XCUIElementTypeTextField" {
                    return Err(Error::Screen.into());
                }
                let is_cancel =
                    name.as_deref() == Some("Cancel") || label.as_deref() == Some("Cancel");
                if is_cancel {
                    if !inside
                        || kind.as_ref() != b"XCUIElementTypeButton"
                        || candidate.is_some()
                        || enabled != Some(true)
                        || [name.as_deref(), label.as_deref()]
                            .into_iter()
                            .flatten()
                            .any(|s| {
                                matches!(s, "Emergency" | "Delete")
                                    || (s.len() == 1 && s.as_bytes()[0].is_ascii_digit())
                            })
                    {
                        return Err(Error::Screen.into());
                    }
                    let visible = visible.ok_or(Error::Screen)?;
                    if !visible && !supervised {
                        return Err(Error::Supervision.into());
                    }
                    candidate = Some((Rect::read(rect, screen)?, visible));
                }
                // Hidden or disabled competitors still cannot prove a safe hit target.
                // Missing or malformed bounds fail closed rather than hiding an overlap.
                if inside && !is_cancel && competing_control(kind.as_ref()) {
                    competing.push(Rect::read(rect, screen).map_err(|_| {
                        CancelFailure::classified(GeometryClass::invalid_competitor(
                            kind.as_ref(),
                            name.as_deref(),
                            label.as_deref(),
                            flags,
                        ))
                    })?);
                }
                if matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeKey" | b"XCUIElementTypeSecureTextField"
                ) {
                    protected.push(Rect::read(rect, screen)?);
                }
                if start {
                    stack.push(inside);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(Error::Screen)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (cancel, visible) = candidate.ok_or(Error::Screen)?;
    let class = classify_rectangles(cancel, &protected, &competing);
    if class != GeometryClass::Stable {
        return Err(CancelFailure::classified(class));
    }
    Ok(Evidence {
        screen,
        image,
        cancel,
        visible,
        protected,
        competing,
    })
}

pub(super) fn competing_control(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"XCUIElementTypeButton"
            | b"XCUIElementTypeLink"
            | b"XCUIElementTypeSwitch"
            | b"XCUIElementTypeSlider"
            | b"XCUIElementTypeStepper"
            | b"XCUIElementTypePicker"
            | b"XCUIElementTypePickerWheel"
            | b"XCUIElementTypeSegmentedControl"
            | b"XCUIElementTypePageIndicator"
            | b"XCUIElementTypeCell"
            | b"XCUIElementTypeTextView"
            | b"XCUIElementTypeSearchField"
    )
}

fn image_size(png: &[u8], screen: (f64, f64)) -> Result<(u32, u32), Error> {
    if png.len() < 24
        || !png.starts_with(b"\x89PNG\r\n\x1a\n")
        || png[8..12] != 13u32.to_be_bytes()
        || &png[12..16] != b"IHDR"
    {
        return Err(Error::Geometry);
    }
    let w = u32::from_be_bytes(png[16..20].try_into().map_err(|_| Error::Geometry)?);
    let h = u32::from_be_bytes(png[20..24].try_into().map_err(|_| Error::Geometry)?);
    if !(1..=6000).contains(&w)
        || !(1..=6000).contains(&h)
        || (f64::from(w) / f64::from(h) - screen.0 / screen.1).abs() > 0.02
    {
        return Err(Error::Geometry);
    }
    Ok((w, h))
}

struct Live {
    observer: LockedObserver,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
}

impl Live {
    fn new(expected: HardwareId) -> Result<Self, Error> {
        Ok(Self {
            observer: LockedObserver::new(expected.clone()).map_err(|_| Error::Capture)?,
            wda: UsbWda::new(PinnedUsb::new(expected)),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Error::Capture)?,
            actions: WdaActions::default(),
        })
    }
}

impl Device for Live {
    fn capture(&mut self) -> Result<LockedCapture, Error> {
        self.observer.capture().map_err(|_| Error::Capture)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn tap(&mut self, session: &str, at: Point, fresh_until: Instant) -> Result<(), ()> {
        let wda = self.wda.with_write_deadline(fresh_until);
        self.runtime.block_on(async {
            if validated_active_session(&wda).await.map_err(|_| ())? != session {
                return Err(());
            }
            self.actions.prepare_existing(&wda).await.map_err(|_| ())?;
            if !self.actions.prepared_session_matches(session) {
                return Err(());
            }
            let receipt = self
                .actions
                .execute_existing(&wda, &Action::Tap { at })
                .await
                .map_err(|_| ())?;
            if receipt.accepted == Some(true) {
                Ok(())
            } else {
                Err(())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::controller::RawObservation;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use handkite_host::wda_settings::SettingsError;
    use std::cell::Cell;
    use std::collections::VecDeque;

    const SESSION: &str = "fixture-session";
    const CANCEL: &str = r#"<XCUIElementTypeButton name="Cancel" visible="true" enabled="true" x="280" y="700" width="100" height="40"/>"#;
    const FIELD: &str = r#"<XCUIElementTypeSecureTextField visible="true" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/>"#;
    const NORMAL: &str = r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" x="0" y="0" width="400" height="800"><XCUIElementTypeOther name="proud-lock" visible="false"/></XCUIElementTypeApplication></AppiumAUT>"#;

    fn id() -> HardwareId {
        HardwareId::new("fixture-phone").unwrap()
    }

    fn keypad() -> String {
        let mut xml = String::from(
            r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" x="0" y="0" width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..10 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="true" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + column * 100, 300 + row * 100));
        }
        xml.push_str(FIELD);
        xml.push_str(CANCEL);
        xml.push_str(r#"<XCUIElementTypeStaticText label="private-canary"/></XCUIElementTypeApplication></AppiumAUT>"#);
        xml
    }

    fn snapshot(base: Instant, offset: u64, xml: String) -> LockedCapture {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        let started = base + Duration::from_millis(offset);
        LockedCapture {
            raw: RawObservation {
                connected_usb: id(),
                wda_target: id(),
                state: DeviceState {
                    lock: LockState::Locked,
                    wda_ready: true,
                    foreground_app: Some("com.apple.springboard".into()),
                },
                window: ObservationWindow {
                    started_unix_ms: 100 + offset,
                    finished_unix_ms: 150 + offset,
                },
                accessibility_tree: xml,
                screenshot: ScreenImage {
                    format: ImageFormat::Png,
                    bytes: png,
                },
            },
            session: SESSION.into(),
            started,
            finished: started + Duration::from_millis(50),
            fresh_until: started + BUDGET,
        }
    }

    struct Fake {
        captures: VecDeque<Result<LockedCapture, Error>>,
        count: usize,
        clock: Cell<Instant>,
        second_reads: Cell<usize>,
        dispatch_delay: Duration,
        receipt: Result<(), ()>,
        taps: Vec<(String, Point, Instant)>,
    }

    impl Fake {
        fn new() -> Self {
            let base = Instant::now();
            Self {
                captures: [
                    Ok(snapshot(base, 0, keypad())),
                    Ok(snapshot(base, 100, keypad())),
                    Ok(snapshot(base, 200, NORMAL.into())),
                ]
                .into(),
                count: 0,
                clock: Cell::new(base),
                second_reads: Cell::new(0),
                dispatch_delay: Duration::ZERO,
                receipt: Ok(()),
                taps: Vec::new(),
            }
        }
        fn at(&mut self, n: usize) -> &mut LockedCapture {
            self.captures[n].as_mut().unwrap()
        }
    }

    impl Device for Fake {
        fn capture(&mut self) -> Result<LockedCapture, Error> {
            self.count += 1;
            let next = self.captures.pop_front().expect("no extra capture");
            if let Ok(snapshot) = &next {
                self.clock.set(snapshot.finished);
            }
            next
        }
        fn now(&self) -> Instant {
            if self.count == 2 {
                self.second_reads.set(self.second_reads.get() + 1);
                if self.second_reads.get() == 2 {
                    self.clock.set(self.clock.get() + self.dispatch_delay);
                }
            }
            self.clock.get()
        }
        fn tap(&mut self, session: &str, at: Point, fresh_until: Instant) -> Result<(), ()> {
            assert!(self.taps.is_empty(), "no retry");
            self.taps.push((session.into(), at, fresh_until));
            self.receipt
        }
    }

    impl ReadOnly for Fake {
        fn capture(&mut self) -> Result<LockedCapture, Error> {
            Device::capture(self)
        }
        fn now(&self) -> Instant {
            Device::now(self)
        }
    }

    #[test]
    fn read_only_geometry_reports_the_existing_guard_category_without_input() {
        let extra = r#"<XCUIElementTypeButton name="private-canary" visible="false" enabled="true" x="280" y="700" width="100" height="40"/>"#;
        for (xml, class) in [
            (
                keypad().replace(CANCEL, &CANCEL.replace("y=\"700\"", "y=\"300\"")),
                GeometryClass::OverlapKeyOrField,
            ),
            (
                keypad().replace(CANCEL, &CANCEL.replace("y=\"700\"", "y=\"220\"")),
                GeometryClass::OverlapKeyOrField,
            ),
            (
                keypad().replace(CANCEL, &format!("{CANCEL}{extra}")),
                GeometryClass::OverlapOtherControl,
            ),
            (
                keypad().replace(
                    CANCEL,
                    &format!("{CANCEL}{}", extra.replace("width=\"100\"", "")),
                ),
                GeometryClass::CompetitorBoundsInvalid(
                    CompetitorKind::Button,
                    NameClass::Other,
                    CompetitorFlags {
                        enabled: AttributeFlag::True,
                        visible: AttributeFlag::False,
                    },
                ),
            ),
        ] {
            for n in 0..2 {
                let mut fake = Fake::new();
                fake.at(n).raw.accessibility_tree = xml.clone();
                assert_eq!(inspect_geometry(&mut fake, &id(), true), Ok(class));
                assert_eq!(fake.count, n + 1);
                assert!(fake.taps.is_empty());
            }
        }
        let mut fake = Fake::new();
        assert_eq!(
            inspect_geometry(&mut fake, &id(), true),
            Ok(GeometryClass::Stable)
        );
        assert_eq!(fake.count, 2);
        assert!(fake.taps.is_empty());
        // The strict keypad parser already rejects count mismatches. Keep the
        // downstream defensive classifier tested without weakening that parser.
        assert_eq!(
            classify_rectangles(Rect([0.0, 0.0, 1.0, 1.0]), &[], &[]),
            GeometryClass::ProtectedCountInvalid
        );
    }

    #[test]
    fn first_invalid_competitor_has_only_fixed_kind_and_name_classes() {
        for (kind, kind_token) in [("Button", "button"), ("Switch", "other_control")] {
            for (attrs, name_token) in [
                (r#"name="Emergency""#, "emergency"),
                (r#"label="Delete""#, "delete"),
                (r#"name="private-canary" label="private-label""#, "other"),
                (r#"name="Emergency" label="Delete""#, "other"),
            ] {
                let first = format!(
                    r#"<XCUIElementType{kind} {attrs} visible="false" enabled="true" x="20" y="700" height="40"/>"#
                );
                let later = r#"<XCUIElementTypeButton name="Delete" x="bad"/>"#;
                for n in 0..2 {
                    let mut fake = Fake::new();
                    fake.at(n).raw.accessibility_tree =
                        keypad().replace(CANCEL, &format!("{CANCEL}{first}{later}"));
                    let class = inspect_geometry(&mut fake, &id(), true).unwrap();
                    assert_eq!(
                        class.line(),
                        format!(
                            "cancel_geometry=observed classification=competitor_bounds_invalid kind={kind_token} name_class={name_token} enabled=true visible=false"
                        )
                    );
                    for private in ["private-canary", "private-label", "700", "40", "<", SESSION] {
                        assert!(!class.line().contains(private));
                    }
                    assert_eq!(fake.count, n + 1);
                    assert!(fake.taps.is_empty());
                    let mut trial = Fake::new();
                    trial.at(n).raw.accessibility_tree =
                        keypad().replace(CANCEL, &format!("{CANCEL}{first}{later}"));
                    assert_eq!(run(&mut trial, &id(), SESSION, true), Err(Error::Geometry));
                    assert!(trial.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn first_invalid_competitor_flags_are_fixed_and_do_not_relax_the_guard() {
        let values = [
            (Some("true"), "true"),
            (Some("false"), "false"),
            (None, "missing"),
            (Some("private-flag"), "missing"),
        ];
        for (enabled, enabled_token) in values {
            for (visible, visible_token) in values {
                let mut first = String::from(
                    r#"<XCUIElementTypeButton name="private-canary" x="20" y="700" height="40""#,
                );
                for (key, value) in [("enabled", enabled), ("visible", visible)] {
                    if let Some(value) = value {
                        first.push_str(&format!(r#" {key}="{value}""#));
                    }
                }
                first.push_str("/>");
                let later =
                    r#"<XCUIElementTypeButton name="Delete" enabled="false" visible="true"/>"#;
                for n in 0..2 {
                    let xml = keypad().replace(CANCEL, &format!("{CANCEL}{first}{later}"));
                    let mut fake = Fake::new();
                    fake.at(n).raw.accessibility_tree = xml.clone();
                    let line = inspect_geometry(&mut fake, &id(), true).unwrap().line();
                    assert_eq!(
                        line,
                        format!(
                            "cancel_geometry=observed classification=competitor_bounds_invalid kind=button name_class=other enabled={enabled_token} visible={visible_token}"
                        )
                    );
                    for private in ["private-canary", "private-flag", "700", "40", "<", SESSION] {
                        assert!(!line.contains(private));
                    }
                    assert_eq!(fake.count, n + 1);
                    assert!(fake.taps.is_empty());
                    let mut trial = Fake::new();
                    trial.at(n).raw.accessibility_tree = xml;
                    assert_eq!(run(&mut trial, &id(), SESSION, true), Err(Error::Geometry));
                    assert!(trial.taps.is_empty());
                }
            }
        }
    }

    #[test]
    fn read_only_geometry_retains_capture_session_and_stability_guards() {
        for (change, expected) in [
            (
                (|s: &mut LockedCapture| s.session = "changed-session".into())
                    as fn(&mut LockedCapture),
                Error::Session,
            ),
            (|s| s.finished = s.fresh_until, Error::Freshness),
            (
                |s| s.raw.wda_target = HardwareId::new("other").unwrap(),
                Error::Screen,
            ),
            (|s| s.raw.state.lock = LockState::Unlocked, Error::Screen),
            (
                |s| s.raw.accessibility_tree = keypad().replace(CANCEL, ""),
                Error::Screen,
            ),
            (
                |s| {
                    s.raw.accessibility_tree =
                        keypad().replace("x=\"280\" y=\"700\"", "x=\"279\" y=\"700\"")
                },
                Error::Geometry,
            ),
            (
                |s| {
                    s.started -= Duration::from_millis(100);
                    s.fresh_until -= Duration::from_millis(100);
                },
                Error::Freshness,
            ),
        ] {
            let mut fake = Fake::new();
            change(fake.at(1));
            assert_eq!(inspect_geometry(&mut fake, &id(), true), Err(expected));
            assert!(fake.taps.is_empty());
        }
        let mut fake = Fake::new();
        fake.captures[0] = Err(Error::Capture);
        assert_eq!(
            inspect_geometry(&mut fake, &id(), true),
            Err(Error::Capture)
        );
        let mut fake = Fake::new();
        fake.at(0).raw.accessibility_tree =
            keypad().replace("visible=\"true\"", "visible=\"false\"");
        assert_eq!(
            inspect_geometry(&mut fake, &id(), false),
            Err(Error::Supervision)
        );
        assert!(fake.taps.is_empty());
    }

    #[test]
    fn geometry_classification_output_is_fixed_and_private() {
        for (class, token) in [
            (
                GeometryClass::ProtectedCountInvalid,
                "protected_count_invalid",
            ),
            (GeometryClass::OverlapKeyOrField, "overlap_key_or_field"),
            (GeometryClass::OverlapOtherControl, "overlap_other_control"),
            (
                GeometryClass::CompetitorBoundsInvalid(
                    CompetitorKind::Button,
                    NameClass::Other,
                    CompetitorFlags {
                        enabled: AttributeFlag::True,
                        visible: AttributeFlag::False,
                    },
                ),
                "competitor_bounds_invalid kind=button name_class=other enabled=true visible=false",
            ),
            (GeometryClass::Stable, "stable"),
        ] {
            let line = class.line();
            assert_eq!(
                line,
                format!("cancel_geometry=observed classification={token}")
            );
            for private in [
                SESSION,
                "fixture-phone",
                "private-canary",
                "Cancel",
                "0 of 6 digits",
                "<",
                "280",
                "700",
            ] {
                assert!(!line.contains(private));
            }
        }
        assert_eq!(
            geometry_command(&[]),
            Err("refused reason=invalid_arguments")
        );
        assert_eq!(
            geometry_command(&["private-canary".into()]),
            Err("refused reason=invalid_arguments")
        );
    }

    #[test]
    fn one_cancel_tap_uses_second_deadline_and_one_postcheck() {
        for hidden in [false, true] {
            let mut fake = Fake::new();
            if hidden {
                for n in 0..2 {
                    fake.at(n).raw.accessibility_tree =
                        keypad().replace("visible=\"true\"", "visible=\"false\"");
                }
            }
            let deadline = fake.at(1).fresh_until;
            assert_eq!(run(&mut fake, &id(), SESSION, hidden), Ok(()));
            assert_eq!(
                fake.taps,
                [(SESSION.into(), Point { x: 330.0, y: 720.0 }, deadline)]
            );
            assert_eq!(fake.count, 3);
            assert!(fake.captures.is_empty());
        }
    }

    #[test]
    fn hidden_cancel_requires_explicit_supervision_in_each_capture() {
        for n in 0..2 {
            let mut fake = Fake::new();
            fake.at(n).raw.accessibility_tree = keypad().replace(
                CANCEL,
                &CANCEL.replace("visible=\"true\"", "visible=\"false\""),
            );
            assert_eq!(
                run(&mut fake, &id(), SESSION, false),
                Err(Error::Supervision)
            );
            assert!(fake.taps.is_empty());
        }
    }

    #[test]
    fn malformed_ambiguous_nonempty_or_overlapping_targets_never_tap() {
        let xml = keypad();
        for invalid in [
            xml.replace(CANCEL, ""),
            xml.replace(CANCEL, &format!("{CANCEL}{CANCEL}")),
            xml.replace(
                CANCEL,
                &CANCEL.replace("enabled=\"true\"", "enabled=\"false\""),
            ),
            xml.replace(CANCEL, &CANCEL.replace("visible=\"true\"", "")),
            xml.replace(
                CANCEL,
                &CANCEL.replace("visible=\"true\"", "visible=\"invalid\""),
            ),
            xml.replace(
                CANCEL,
                &CANCEL.replace("visible=\"true\"", "visible=\"true\" visible=\"false\""),
            ),
            xml.replace(CANCEL, &CANCEL.replace("x=\"280\"", "x=\"NaN\"")),
            xml.replace(CANCEL, &CANCEL.replace("x=\"280\"", "x=\"350\"")),
            xml.replace(CANCEL, &CANCEL.replace("y=\"700\"", "y=\"300\"")),
            xml.replace(CANCEL, &CANCEL.replace("y=\"700\"", "y=\"220\"")),
            xml.replace(
                CANCEL,
                &CANCEL.replace("name=\"Cancel\"", "name=\"Cancel\" label=\"Emergency\""),
            ),
            xml.replace(
                CANCEL,
                &CANCEL.replace("name=\"Cancel\"", "name=\"Cancel\" label=\"Delete\""),
            ),
            xml.replace(FIELD, &format!("{FIELD}{FIELD}")),
            xml.replace("0 of 6 digits", "1 of 6 digits"),
            xml.replace("0 of 6 digits", "0 of 4 digits"),
            xml.replace("name=\"9\"", "name=\"1\""),
            xml.replace(CANCEL, "<XCUIElementTypeAlert/>"),
            xml.replace(CANCEL, "<XCUIElementTypeTextField/>"),
            xml.replace("</AppiumAUT>", &format!("{CANCEL}</AppiumAUT>")),
            xml.replace("</AppiumAUT>", ""),
            xml.replace("name=\"SpringBoard\"", "name=\"OtherApp\""),
            xml.replace("x=\"0\" y=\"0\"", "x=\"1\" y=\"0\""),
        ] {
            let mut fake = Fake::new();
            fake.at(0).raw.accessibility_tree = invalid;
            assert!(run(&mut fake, &id(), SESSION, true).is_err());
            assert!(fake.taps.is_empty());
            assert_eq!(fake.count, 1);
        }
    }

    #[test]
    fn competing_buttons_and_controls_must_prove_nonoverlap_in_both_captures() {
        for kind in [
            "Button",
            "Link",
            "Switch",
            "Slider",
            "Stepper",
            "Picker",
            "PickerWheel",
            "SegmentedControl",
            "PageIndicator",
            "Cell",
            "TextView",
            "SearchField",
        ] {
            for name in ["Emergency", "Delete", "Other"] {
                for bounds in [
                    r#"x="280" y="700" width="100" height="40""#,
                    r#"x="bad" y="700" width="100" height="40""#,
                    r#"x="20" y="700" height="40""#,
                ] {
                    for n in 0..2 {
                        let extra = format!(
                            r#"<XCUIElementType{kind} name="{name}" visible="false" enabled="false" {bounds}/>"#
                        );
                        let mut fake = Fake::new();
                        fake.at(n).raw.accessibility_tree =
                            keypad().replace(CANCEL, &format!("{CANCEL}{extra}"));
                        assert_eq!(run(&mut fake, &id(), SESSION, true), Err(Error::Geometry));
                        assert!(fake.taps.is_empty());
                        assert_eq!(fake.count, n + 1);
                    }
                }
            }
        }
        let extra = r#"<XCUIElementTypeButton name="Emergency" visible="false" enabled="true" x="20" y="700" width="100" height="40"/>"#;
        let mut fake = Fake::new();
        for n in 0..2 {
            fake.at(n).raw.accessibility_tree =
                keypad().replace(CANCEL, &format!("{CANCEL}{extra}"));
        }
        assert_eq!(run(&mut fake, &id(), SESSION, true), Ok(()));
        assert_eq!(fake.taps.len(), 1);
        let mut fake = Fake::new();
        fake.at(1).raw.accessibility_tree = keypad().replace(CANCEL, &format!("{CANCEL}{extra}"));
        assert_eq!(run(&mut fake, &id(), SESSION, true), Err(Error::Geometry));
        assert!(fake.taps.is_empty());
    }

    #[test]
    fn session_identity_lock_capture_and_freshness_refusals_never_tap() {
        type Change = fn(&mut LockedCapture);
        for change in [
            (|s: &mut LockedCapture| s.session = "another-valid-session".into()) as Change,
            |s| s.raw.connected_usb = HardwareId::new("other").unwrap(),
            |s| s.raw.wda_target = HardwareId::new("other").unwrap(),
            |s| s.raw.state.wda_ready = false,
            |s| s.raw.state.lock = LockState::Unlocked,
            |s| s.raw.state.foreground_app = Some("other".into()),
            |s| s.finished = s.fresh_until,
            |s| s.fresh_until += Duration::from_nanos(1),
            |s| s.raw.window.finished_unix_ms = 0,
            |s| s.raw.screenshot.bytes.clear(),
        ] {
            for n in 0..2 {
                let mut fake = Fake::new();
                change(fake.at(n));
                assert!(run(&mut fake, &id(), SESSION, true).is_err());
                assert!(fake.taps.is_empty());
            }
        }
        for n in 0..2 {
            let mut fake = Fake::new();
            fake.captures[n] = Err(Error::Capture);
            assert_eq!(run(&mut fake, &id(), SESSION, true), Err(Error::Capture));
            assert!(fake.taps.is_empty());
        }
    }

    #[test]
    fn inconsistent_or_overlapping_precaptures_never_tap() {
        for change in [
            (|s: &mut LockedCapture| {
                s.raw.accessibility_tree =
                    keypad().replace("x=\"280\" y=\"700\"", "x=\"279\" y=\"700\"")
            }) as fn(&mut LockedCapture),
            |s| {
                s.raw.accessibility_tree = keypad().replace(
                    CANCEL,
                    &CANCEL.replace("visible=\"true\"", "visible=\"false\""),
                )
            },
            |s| {
                s.started -= Duration::from_millis(100);
                s.fresh_until -= Duration::from_millis(100);
            },
        ] {
            let mut fake = Fake::new();
            change(fake.at(1));
            assert!(run(&mut fake, &id(), SESSION, true).is_err());
            assert!(fake.taps.is_empty());
        }
    }

    #[test]
    fn first_may_expire_but_second_deadline_never_renews() {
        let mut fake = Fake::new();
        let base = fake.at(0).started;
        fake.captures[1] = Ok(snapshot(base, 4900, keypad()));
        fake.at(1).finished = base + Duration::from_millis(5100);
        fake.captures[2] = Ok(snapshot(base, 5200, NORMAL.into()));
        let deadline = fake.at(1).fresh_until;
        assert_eq!(run(&mut fake, &id(), SESSION, true), Ok(()));
        assert_eq!(fake.taps[0].2, deadline);
        for extra in [0, 1] {
            let mut fake = Fake::new();
            fake.dispatch_delay = Duration::from_millis(4950 + extra);
            assert_eq!(run(&mut fake, &id(), SESSION, true), Err(Error::Freshness));
            assert!(fake.taps.is_empty());
        }
    }

    #[test]
    fn post_failures_and_uncertain_receipts_check_once_and_never_replay() {
        for change in [
            (|s: &mut LockedCapture| s.raw.accessibility_tree = keypad()) as fn(&mut LockedCapture),
            |s| s.session = "another-valid-session".into(),
            |s| s.raw.state.lock = LockState::Unlocked,
            |s| s.raw.wda_target = HardwareId::new("other").unwrap(),
            |s| s.finished = s.fresh_until,
            |s| {
                s.raw.accessibility_tree = NORMAL.replace(
                    "</XCUIElementTypeApplication>",
                    "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
                )
            },
            |s| s.raw.screenshot.bytes.clear(),
            |s| {
                s.started -= Duration::from_millis(200);
                s.fresh_until -= Duration::from_millis(200);
            },
        ] {
            let mut fake = Fake::new();
            change(fake.at(2));
            assert_eq!(run(&mut fake, &id(), SESSION, true), Err(Error::Unknown));
            assert_eq!(fake.count, 3);
            assert_eq!(fake.taps.len(), 1);
        }
        for receipt in [Ok(()), Err(())] {
            for capture_failure in [false, true] {
                let mut fake = Fake::new();
                fake.receipt = receipt;
                if capture_failure {
                    fake.captures[2] = Err(Error::Capture);
                }
                let result = run(&mut fake, &id(), SESSION, true);
                assert_eq!(
                    result,
                    if receipt.is_ok() && !capture_failure {
                        Ok(())
                    } else {
                        Err(Error::Unknown)
                    }
                );
                assert_eq!(fake.count, 3);
                assert_eq!(fake.taps.len(), 1);
            }
        }
    }

    #[test]
    fn restoration_precedence_and_outputs_are_fixed() {
        for result in [Ok(()), Err(Error::Unknown), Err(Error::Screen)] {
            for restoration in [
                Restoration::Verified,
                Restoration::NotNeeded,
                Restoration::Unproven(SettingsError::WriteUnproven),
            ] {
                let expected = if restoration == Restoration::Verified {
                    result.map_err(Error::message)
                } else {
                    Err("unknown")
                };
                assert_eq!(
                    idle_result(IdleSettingsOutcome {
                        action: IdleAction::Completed(result),
                        restoration
                    }),
                    expected
                );
            }
        }
        for restoration in [Restoration::Verified, Restoration::NotNeeded] {
            assert_eq!(
                idle_result(IdleSettingsOutcome {
                    action: IdleAction::NotRun(SettingsError::SessionUnproven),
                    restoration
                }),
                Err("refused reason=settings_unproven")
            );
        }
        for (error, expected) in [
            (Error::Capture, "refused reason=capture_unproven"),
            (Error::Session, "refused reason=session_unproven"),
            (Error::Freshness, "refused reason=freshness_unproven"),
            (Error::Screen, "refused reason=screen_unproven"),
            (Error::Geometry, "refused reason=geometry_unproven"),
            (Error::Supervision, "refused reason=supervision_required"),
            (Error::Unknown, "unknown"),
        ] {
            assert_eq!(error.message(), expected);
            for private in [
                SESSION,
                "fixture-phone",
                "private-canary",
                "0 of 6 digits",
                "<",
                "330",
                "720",
            ] {
                assert!(!error.message().contains(private));
            }
        }
    }

    #[tokio::test]
    async fn blocking_worker_is_joined_and_panics_cannot_report_success() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        assert_eq!(
            blocking_trial(move || {
                worker_completed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await,
            Ok(())
        );
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(
            blocking_trial(|| panic!("fixture worker failure")).await,
            Err(Error::Unknown)
        );
    }
}
