use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{HardwareId, LockState};
use handkite_host::native_lock::NativeLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_action::WdaActions;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_http::HttpResponse;
use handkite_host::wda_transport::{UsbWda, WdaError};
use handkite_host::{host_config, native_lock, pin_screen};
use quick_xml::events::Event;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(5);
const SPRINGBOARD: &str = "com.apple.springboard";
const RETURN_BODY: &[u8] = br#"{"page":7,"usage":40,"duration":0.005}"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    Identity,
    Session,
    NativeLock,
    FirstUnlock,
    WdaLock,
    Screen,
    Geometry,
    Freshness,
    Capture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnknownReason {
    ReceiptUnproven,
    PostcaptureFailed,
    PostFreshnessUnproven,
    PostconditionUnproven,
    SessionChanged,
    PostOrderUnproven,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Error {
    Refused(Refusal),
    Unknown(UnknownReason),
}

impl Error {
    fn message(self) -> &'static str {
        match self {
            Self::Refused(Refusal::Identity) => "refused reason=identity_unproven",
            Self::Refused(Refusal::Session) => "refused reason=session_unproven",
            Self::Refused(Refusal::NativeLock) => "refused reason=native_lock_unproven",
            Self::Refused(Refusal::FirstUnlock) => "refused reason=first_unlock_unproven",
            Self::Refused(Refusal::WdaLock) => "refused reason=wda_lock_unproven",
            Self::Refused(Refusal::Screen) => "refused reason=screen_unproven",
            Self::Refused(Refusal::Geometry) => "refused reason=geometry_unproven",
            Self::Refused(Refusal::Freshness) => "refused reason=freshness_unproven",
            Self::Refused(Refusal::Capture) => "refused reason=capture_failed",
            Self::Unknown(UnknownReason::ReceiptUnproven) => "unknown reason=receipt_unproven",
            Self::Unknown(UnknownReason::PostcaptureFailed) => "unknown reason=postcapture_failed",
            Self::Unknown(UnknownReason::PostFreshnessUnproven) => {
                "unknown reason=post_freshness_unproven"
            }
            Self::Unknown(UnknownReason::PostconditionUnproven) => {
                "unknown reason=postcondition_unproven"
            }
            Self::Unknown(UnknownReason::SessionChanged) => "unknown reason=session_changed",
            Self::Unknown(UnknownReason::PostOrderUnproven) => "unknown reason=post_order_unproven",
        }
    }
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("invalid_arguments");
    };
    if flag != "--config" {
        return Err("invalid_arguments");
    }
    let binding = host_config::load(Path::new(path)).map_err(|_| "configuration_invalid")?;
    let mut live = Live::new(binding.usb_id.clone()).map_err(|r| Error::Refused(r).message())?;
    run(&mut live, &binding.usb_id).map_err(Error::message)?;
    println!("reveal_keypad=confirmed");
    Ok(())
}

struct Snapshot {
    raw: RawObservation,
    native: NativeLock,
    wda_locked: Option<bool>,
    session: String,
    started: Instant,
    finished: Instant,
    fresh_until: Instant,
}

trait Device {
    fn capture(&mut self) -> Result<Snapshot, Refusal>;
    fn now(&self) -> Instant;
    fn reveal(&mut self, session: &str, fresh_until: Instant) -> Result<(), ()>;
}

fn run(device: &mut impl Device, expected: &HardwareId) -> Result<(), Error> {
    let before = device.capture().map_err(Error::Refused)?;
    let first_evidence =
        validate(&before, expected, device.now(), false).map_err(Error::Refused)?;
    let current = device.capture().map_err(Error::Refused)?;
    let second_evidence =
        validate(&current, expected, device.now(), false).map_err(Error::Refused)?;
    if current.session != before.session {
        return Err(Error::Refused(Refusal::Session));
    }
    if current.started < before.finished
        || current.started <= before.started
        || current.finished <= before.finished
    {
        return Err(Error::Refused(Refusal::Freshness));
    }
    if first_evidence.proud_visible != second_evidence.proud_visible {
        return Err(Error::Refused(Refusal::Screen));
    }
    if first_evidence != second_evidence {
        return Err(Error::Refused(Refusal::Geometry));
    }
    let input_started = device.now();
    if input_started >= current.fresh_until {
        return Err(Error::Refused(Refusal::Freshness));
    }
    let receipt = device.reveal(&current.session, current.fresh_until);
    // Once dispatch is entered, all failures are unknown. Observe once, never replay.
    let captured = device.capture();
    if receipt.is_err() {
        return Err(Error::Unknown(UnknownReason::ReceiptUnproven));
    }
    let after = captured.map_err(|_| Error::Unknown(UnknownReason::PostcaptureFailed))?;
    validate(&after, expected, device.now(), true).map_err(|reason| {
        Error::Unknown(if reason == Refusal::Freshness {
            UnknownReason::PostFreshnessUnproven
        } else {
            UnknownReason::PostconditionUnproven
        })
    })?;
    if after.session != current.session {
        return Err(Error::Unknown(UnknownReason::SessionChanged));
    }
    if after.started < input_started {
        return Err(Error::Unknown(UnknownReason::PostOrderUnproven));
    }
    Ok(())
}

#[derive(PartialEq)]
struct Evidence {
    screen: (f64, f64),
    image: (u32, u32),
    proud_visible: Option<bool>,
}

#[derive(Debug, PartialEq)]
struct NormalLockScreen {
    size: (f64, f64),
    proud_visible: bool,
}

fn native_locked(native: NativeLock) -> Result<(), Refusal> {
    if native.state != LockState::Locked {
        return Err(Refusal::NativeLock);
    }
    if native.unlocked_since_boot != Some(true) {
        return Err(Refusal::FirstUnlock);
    }
    Ok(())
}

fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate(
    snapshot: &Snapshot,
    expected: &HardwareId,
    now: Instant,
    after: bool,
) -> Result<Evidence, Refusal> {
    if snapshot.raw.connected_usb != *expected || snapshot.raw.wda_target != *expected {
        return Err(Refusal::Identity);
    }
    if !valid_session(&snapshot.session) || !snapshot.raw.state.wda_ready {
        return Err(Refusal::Session);
    }
    native_locked(snapshot.native)?;
    if snapshot.wda_locked != Some(true) || snapshot.raw.state.lock != LockState::Locked {
        return Err(Refusal::WdaLock);
    }
    if snapshot.raw.state.foreground_app.as_deref() != Some(SPRINGBOARD) {
        return Err(Refusal::Screen);
    }
    if snapshot.started > snapshot.finished
        || snapshot.finished > now
        || now >= snapshot.fresh_until
        || snapshot.started.checked_add(BUDGET) != Some(snapshot.fresh_until)
    {
        return Err(Refusal::Freshness);
    }
    let xml = &snapshot.raw.accessibility_tree;
    let (size, proud_visible) = if after {
        if !super::lock_ui_attrs::empty_keypad(xml) {
            return Err(Refusal::Screen);
        }
        let size = pin_screen::parse(xml)
            .map_err(|_| Refusal::Screen)?
            .screen_size
            .ok_or(Refusal::Screen)?;
        (size, None)
    } else {
        let normal = normal_lock_screen(xml)?;
        (normal.size, Some(normal.proud_visible))
    };
    let png = &snapshot.raw.screenshot.bytes;
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || &png[12..16] != b"IHDR" {
        return Err(Refusal::Capture);
    }
    let width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| Refusal::Capture)?);
    let height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| Refusal::Capture)?);
    if width == 0
        || height == 0
        || (f64::from(width) / f64::from(height) - size.0 / size.1).abs() > 0.02
    {
        return Err(Refusal::Capture);
    }
    Ok(Evidence {
        screen: size,
        image: (width, height),
        proud_visible,
    })
}

pub(super) fn normal_lock_size(xml: &str) -> Option<(f64, f64)> {
    normal_lock_screen(xml).ok().map(|screen| screen.size)
}

fn normal_lock_screen(xml: &str) -> Result<NormalLockScreen, Refusal> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(Refusal::Screen);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let (mut roots, mut apps, mut proud) = (0, 0, 0);
    let mut size = None;
    let mut proud_visible = None;
    loop {
        let event = reader.read_event().map_err(|_| Refusal::Screen)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() >= 128 {
                    return Err(Refusal::Screen);
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(Refusal::Screen);
                    }
                }
                let (mut name, mut label, mut bundle) = (None, None, None);
                let (mut width, mut height) = (None, None);
                let mut visible = None;
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| Refusal::Screen)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| Refusal::Screen)?;
                    match attr.key.as_ref() {
                        b"name" => name = Some(value.into_owned()),
                        b"label" => label = Some(value.into_owned()),
                        b"bundleId" => bundle = Some(value == SPRINGBOARD),
                        b"visible" => {
                            visible = match value.as_ref() {
                                "true" => Some(true),
                                "false" => Some(false),
                                _ => None,
                            };
                        }
                        b"width" => width = value.parse::<f64>().ok(),
                        b"height" => height = value.parse::<f64>().ok(),
                        _ => {}
                    }
                }
                let kind = node.name();
                if matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeAlert"
                        | b"XCUIElementTypeSheet"
                        | b"XCUIElementTypeSecureTextField"
                        | b"XCUIElementTypeTextField"
                        | b"XCUIElementTypeKey"
                        | b"XCUIElementTypeIcon"
                ) || [name.as_deref(), label.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|value| {
                        matches!(
                            value,
                            "passcode-view"
                                | "Enter Passcode"
                                | "Trust"
                                | "Trust This Computer"
                                | "Trust This Computer?"
                        ) || (value.len() == 1 && value.as_bytes()[0].is_ascii_digit())
                    })
                {
                    return Err(Refusal::Screen);
                }
                let mut inside = stack.last().copied().unwrap_or(false);
                if kind.as_ref() == b"XCUIElementTypeApplication" {
                    apps += 1;
                    if apps != 1
                        || !(bundle == Some(true)
                            || (bundle.is_none()
                                && (name.as_deref() == Some("SpringBoard")
                                    || label.as_deref() == Some("SpringBoard"))))
                    {
                        return Err(Refusal::Screen);
                    }
                    inside = true;
                    size = width.zip(height).filter(|(w, h)| {
                        (200.0..=2000.0).contains(w) && (400.0..=2000.0).contains(h)
                    });
                }
                if name.as_deref() == Some("proud-lock") {
                    if !inside || kind.as_ref() != b"XCUIElementTypeOther" || visible.is_none() {
                        return Err(Refusal::Screen);
                    }
                    proud_visible = visible;
                    proud += 1;
                }
                if start {
                    stack.push(inside);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(Refusal::Screen)?;
            }
            Event::Text(text) if text.as_ref().iter().any(|b| !b.is_ascii_whitespace()) => {
                return Err(Refusal::Screen);
            }
            Event::DocType(_) | Event::CData(_) | Event::GeneralRef(_) => {
                return Err(Refusal::Screen);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || apps != 1 || proud != 1 || !stack.is_empty() {
        return Err(Refusal::Screen);
    }
    Ok(NormalLockScreen {
        size: size.ok_or(Refusal::Screen)?,
        proud_visible: proud_visible.ok_or(Refusal::Screen)?,
    })
}

trait HidTransport {
    async fn post(
        &self,
        path: &str,
        body: &[u8],
        fresh_until: Instant,
    ) -> Result<HttpResponse, WdaError>;
}

impl HidTransport for UsbWda {
    async fn post(
        &self,
        path: &str,
        body: &[u8],
        fresh_until: Instant,
    ) -> Result<HttpResponse, WdaError> {
        self.with_write_deadline(fresh_until)
            .request("POST", path, body, Duration::from_secs(10))
            .await
    }
}

async fn send_return(
    wda: &impl HidTransport,
    session: &str,
    fresh_until: Instant,
) -> Result<(), ()> {
    if !valid_session(session) {
        return Err(());
    }
    let response = wda
        .post(
            &format!("/session/{session}/wda/performIoHidEvent"),
            RETURN_BODY,
            fresh_until,
        )
        .await
        .map_err(|_| ())?;
    response_json(response).map(|_| ()).map_err(|_| ())
}

fn response_json(response: HttpResponse) -> Result<Value, Refusal> {
    let json: Value = serde_json::from_slice(&response.body).map_err(|_| Refusal::Capture)?;
    if response.status != 200 || json.get("value").is_none() || json["value"].get("error").is_some()
    {
        return Err(Refusal::Capture);
    }
    Ok(json)
}

struct Live {
    expected: HardwareId,
    adapter: WdaAdapter,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
    actions: WdaActions,
}

impl Live {
    fn new(expected: HardwareId) -> Result<Self, Refusal> {
        Ok(Self {
            adapter: WdaAdapter::new(expected.clone()).map_err(|_| Refusal::Capture)?,
            wda: UsbWda::new(PinnedUsb::new(expected.clone())),
            expected,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Refusal::Capture)?,
            actions: WdaActions::default(),
        })
    }

    fn get(&self, path: &str) -> Result<Value, Refusal> {
        response_json(
            self.runtime
                .block_on(self.wda.request("GET", path, b"", Duration::from_secs(10)))
                .map_err(|_| Refusal::Capture)?,
        )
    }

    fn session(&mut self) -> Result<String, Refusal> {
        let json = self.get("/status").map_err(|_| Refusal::Session)?;
        let id = json["sessionId"]
            .as_str()
            .filter(|id| valid_session(id))
            .ok_or(Refusal::Session)?;
        if json["value"]["ready"] != true {
            return Err(Refusal::Session);
        }
        self.runtime
            .block_on(self.actions.prepare_existing(&self.wda))
            .map_err(|_| Refusal::Session)?;
        if !self.actions.prepared_session_matches(id) {
            return Err(Refusal::Session);
        }
        Ok(id.to_owned())
    }

    fn locked(&self) -> Result<(), Refusal> {
        if self.get("/wda/locked")?["value"] != true {
            return Err(Refusal::WdaLock);
        }
        if self.get("/wda/activeAppInfo")?["value"]["bundleId"] != SPRINGBOARD {
            return Err(Refusal::Screen);
        }
        Ok(())
    }
}

impl Device for Live {
    fn capture(&mut self) -> Result<Snapshot, Refusal> {
        self.adapter.identities().map_err(|_| Refusal::Identity)?;
        native_locked(native_lock::query(&self.expected))?;
        let session = self.session()?;
        self.locked()?;
        let started = Instant::now();
        let fresh_until = started.checked_add(BUDGET).ok_or(Refusal::Freshness)?;
        let (raw, finished) = self
            .adapter
            .observe_pin_with_attributes()
            .map_err(|_| Refusal::Capture)?;
        self.locked()?;
        if self.session()? != session {
            return Err(Refusal::Session);
        }
        let native = native_lock::query(&self.expected);
        native_locked(native)?;
        self.adapter.identities().map_err(|_| Refusal::Identity)?;
        Ok(Snapshot {
            raw,
            native,
            session,
            wda_locked: Some(true),
            started,
            finished,
            fresh_until,
        })
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn reveal(&mut self, session: &str, fresh_until: Instant) -> Result<(), ()> {
        self.runtime
            .block_on(send_return(&self.wda, session, fresh_until))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    const NORMAL: &str = r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" width="400" height="800"><XCUIElementTypeOther name="proud-lock" visible="true"/></XCUIElementTypeApplication></AppiumAUT>"#;

    fn keypad() -> String {
        let mut xml = String::from(
            r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..=9 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="true" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + column * 100, 300 + row * 100));
        }
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="true" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/></XCUIElementTypeApplication></AppiumAUT>"#);
        xml
    }

    fn id() -> HardwareId {
        HardwareId::new("fixture-device").unwrap()
    }

    fn snapshot(started: Instant, after: bool) -> Snapshot {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1200u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        Snapshot {
            raw: RawObservation {
                connected_usb: id(),
                wda_target: id(),
                window: ObservationWindow {
                    started_unix_ms: 1,
                    finished_unix_ms: 2,
                },
                state: DeviceState {
                    lock: LockState::Locked,
                    wda_ready: true,
                    foreground_app: Some(SPRINGBOARD.into()),
                },
                accessibility_tree: if after { keypad() } else { NORMAL.into() },
                screenshot: ScreenImage {
                    format: ImageFormat::Png,
                    bytes: png,
                },
            },
            native: NativeLock {
                state: LockState::Locked,
                unlocked_since_boot: Some(true),
            },
            wda_locked: Some(true),
            session: "fixture-session".into(),
            started,
            finished: started + Duration::from_millis(100),
            fresh_until: started + BUDGET,
        }
    }

    struct Fake {
        captures: VecDeque<Result<Snapshot, Refusal>>,
        captures_called: usize,
        posts: Vec<(String, Instant)>,
        receipt: Result<(), ()>,
        now: Instant,
        now_calls: Cell<usize>,
        override_now: Option<(usize, Instant)>,
    }

    impl Fake {
        fn new() -> Self {
            let now = Instant::now();
            Self {
                captures: VecDeque::from([
                    Ok(snapshot(now, false)),
                    Ok(snapshot(now + Duration::from_secs(1), false)),
                    Ok(snapshot(now + Duration::from_secs(2), true)),
                ]),
                captures_called: 0,
                posts: Vec::new(),
                receipt: Ok(()),
                now,
                now_calls: Cell::new(0),
                override_now: None,
            }
        }
    }

    impl Device for Fake {
        fn capture(&mut self) -> Result<Snapshot, Refusal> {
            self.captures_called += 1;
            let result = self.captures.pop_front().expect("unexpected extra capture");
            if let Ok(snapshot) = &result {
                self.now = snapshot.finished + Duration::from_millis(100);
            }
            result
        }
        fn now(&self) -> Instant {
            let call = self.now_calls.get() + 1;
            self.now_calls.set(call);
            self.override_now
                .filter(|(index, _)| *index == call)
                .map_or(self.now, |(_, now)| now)
        }
        fn reveal(&mut self, session: &str, fresh_until: Instant) -> Result<(), ()> {
            assert!(self.posts.is_empty(), "input replayed");
            self.posts.push((session.into(), fresh_until));
            self.receipt
        }
    }

    #[test]
    fn exactly_one_dispatch_and_one_postcheck_confirm_only_an_empty_keypad() {
        for (proud_visible, hidden) in [(true, false), (true, true), (false, false), (false, true)]
        {
            let mut fake = Fake::new();
            let expiry = fake.captures[1].as_ref().unwrap().fresh_until;
            for index in [0, 1] {
                fake.captures[index]
                    .as_mut()
                    .unwrap()
                    .raw
                    .accessibility_tree =
                    NORMAL.replace("visible=\"true\"", &format!("visible=\"{proud_visible}\""));
            }
            if hidden {
                let after = fake.captures[2].as_mut().unwrap();
                after.raw.accessibility_tree = after
                    .raw
                    .accessibility_tree
                    .replace("visible=\"true\"", "visible=\"false\"");
            }
            assert_eq!(run(&mut fake, &id()), Ok(()));
            assert_eq!(fake.posts, [("fixture-session".into(), expiry)]);
            assert_eq!(fake.captures_called, 3);
            assert!(fake.captures.is_empty());
        }
    }

    #[test]
    fn visibility_must_be_explicit_unique_and_equal_in_both_precaptures() {
        for attribute in [
            "",
            "visible=\"invalid\"",
            "visible=\"0\"",
            "visible=\"TRUE\"",
            "visible=\"true\" visible=\"false\"",
        ] {
            for index in [0, 1] {
                let mut fake = Fake::new();
                fake.captures[index]
                    .as_mut()
                    .unwrap()
                    .raw
                    .accessibility_tree = NORMAL.replace("visible=\"true\"", attribute);
                assert_eq!(run(&mut fake, &id()), Err(Error::Refused(Refusal::Screen)));
                assert!(fake.posts.is_empty());
                assert_eq!(fake.captures_called, index + 1);
            }
        }
        for hidden_index in [0, 1] {
            let mut fake = Fake::new();
            fake.captures[hidden_index]
                .as_mut()
                .unwrap()
                .raw
                .accessibility_tree = NORMAL.replace("visible=\"true\"", "visible=\"false\"");
            assert_eq!(run(&mut fake, &id()), Err(Error::Refused(Refusal::Screen)));
            assert!(fake.posts.is_empty());
            assert_eq!(fake.captures_called, 2);
        }
    }

    #[test]
    fn preflight_guards_fail_closed_without_dispatch_or_postcheck() {
        type Change = fn(&mut Snapshot);
        for (reason, change) in [
            (
                Refusal::Identity,
                (|s: &mut Snapshot| s.raw.connected_usb = HardwareId::new("other").unwrap())
                    as Change,
            ),
            (Refusal::Identity, |s| {
                s.raw.wda_target = HardwareId::new("other").unwrap()
            }),
            (Refusal::Session, |s| s.session = "../other".into()),
            (Refusal::Session, |s| s.raw.state.wda_ready = false),
            (Refusal::NativeLock, |s| {
                s.native.state = LockState::Unlocked
            }),
            (Refusal::NativeLock, |s| {
                s.native.state = LockState::Uncertain
            }),
            (Refusal::FirstUnlock, |s| {
                s.native.unlocked_since_boot = None
            }),
            (Refusal::FirstUnlock, |s| {
                s.native.unlocked_since_boot = Some(false)
            }),
            (Refusal::WdaLock, |s| s.wda_locked = Some(false)),
            (Refusal::WdaLock, |s| s.wda_locked = None),
            (Refusal::WdaLock, |s| {
                s.raw.state.lock = LockState::Uncertain
            }),
            (Refusal::Screen, |s| {
                s.raw.state.foreground_app = Some("com.example.other".into())
            }),
            (Refusal::Screen, |s| s.raw.accessibility_tree = keypad()),
            (Refusal::Capture, |s| s.raw.screenshot.bytes.clear()),
            (Refusal::Capture, |s| {
                s.raw.screenshot.bytes[16..20].copy_from_slice(&1u32.to_be_bytes())
            }),
            (Refusal::Freshness, |s| {
                s.fresh_until += Duration::from_secs(1)
            }),
            (Refusal::Freshness, |s| {
                s.finished = s.started - Duration::from_nanos(1)
            }),
        ] {
            for index in [0, 1] {
                let mut fake = Fake::new();
                change(fake.captures[index].as_mut().unwrap());
                assert_eq!(run(&mut fake, &id()), Err(Error::Refused(reason)));
                assert!(fake.posts.is_empty());
                assert_eq!(fake.captures_called, index + 1);
            }
        }
        for elapsed in [BUDGET, BUDGET + Duration::from_nanos(1)] {
            let mut fake = Fake::new();
            fake.override_now = Some((1, fake.now + elapsed));
            assert_eq!(
                run(&mut fake, &id()),
                Err(Error::Refused(Refusal::Freshness))
            );
            assert!(fake.posts.is_empty());
        }
        for index in [0, 1] {
            let mut fake = Fake::new();
            fake.captures[index] = Err(Refusal::Capture);
            assert_eq!(run(&mut fake, &id()), Err(Error::Refused(Refusal::Capture)));
            assert!(fake.posts.is_empty());
            assert_eq!(fake.captures_called, index + 1);
        }
    }

    #[test]
    fn second_capture_must_be_distinct_nonoverlapping_fresh_and_consistent() {
        for (offset, reason) in [
            (Duration::ZERO, Refusal::Freshness),
            (Duration::from_millis(99), Refusal::Freshness),
        ] {
            let mut fake = Fake::new();
            let started = fake.captures[0].as_ref().unwrap().started;
            fake.captures[1] = Ok(snapshot(started + offset, false));
            assert_eq!(run(&mut fake, &id()), Err(Error::Refused(reason)));
            assert!(fake.posts.is_empty());
            assert_eq!(fake.captures_called, 2);
        }
        type Change = fn(&mut Snapshot);
        for (reason, change) in [
            (
                Refusal::Session,
                (|s: &mut Snapshot| s.session = "replacement".into()) as Change,
            ),
            (Refusal::Geometry, |s| {
                s.raw.accessibility_tree = NORMAL
                    .replace("width=\"400\"", "width=\"500\"")
                    .replace("height=\"800\"", "height=\"1000\"");
            }),
            (Refusal::Geometry, |s| {
                s.raw.screenshot.bytes[16..20].copy_from_slice(&2400u32.to_be_bytes());
                s.raw.screenshot.bytes[20..24].copy_from_slice(&4800u32.to_be_bytes());
            }),
        ] {
            let mut fake = Fake::new();
            change(fake.captures[1].as_mut().unwrap());
            assert_eq!(run(&mut fake, &id()), Err(Error::Refused(reason)));
            assert!(fake.posts.is_empty());
            assert_eq!(fake.captures_called, 2);
        }
        let mut fake = Fake::new();
        let first_finished = fake.captures[0].as_ref().unwrap().finished;
        fake.captures[1] = Ok(snapshot(first_finished, false));
        let expiry = fake.captures[1].as_ref().unwrap().fresh_until;
        assert_eq!(run(&mut fake, &id()), Ok(()));
        assert_eq!(fake.posts, [("fixture-session".into(), expiry)]);
        assert_eq!(fake.captures_called, 3);
    }

    #[test]
    fn only_the_second_capture_deadline_controls_dispatch() {
        let mut fake = Fake::new();
        let started = fake.captures[0].as_ref().unwrap().started;
        let mut second = snapshot(started + BUDGET - Duration::from_secs(1), false);
        second.finished = started + BUDGET + Duration::from_millis(100);
        let expiry = second.fresh_until;
        fake.captures[1] = Ok(second);
        fake.captures[2] = Ok(snapshot(started + BUDGET + Duration::from_secs(1), true));
        assert_eq!(run(&mut fake, &id()), Ok(()));
        assert_eq!(fake.posts, [("fixture-session".into(), expiry)]);
        assert_eq!(fake.captures_called, 3);

        // Expire the second capture either at validation or immediately before dispatch.
        for now_call in [2, 3] {
            for lateness in [Duration::ZERO, Duration::from_nanos(1)] {
                let mut fake = Fake::new();
                let expiry = fake.captures[1].as_ref().unwrap().fresh_until;
                fake.override_now = Some((now_call, expiry + lateness));
                assert_eq!(
                    run(&mut fake, &id()),
                    Err(Error::Refused(Refusal::Freshness))
                );
                assert!(fake.posts.is_empty());
                assert_eq!(fake.captures_called, 2);
            }
        }
    }

    #[test]
    fn normal_screen_rejects_keypads_overlays_ambiguity_and_malformed_xml() {
        for visible in [true, false] {
            assert_eq!(
                normal_lock_screen(
                    &NORMAL.replace("visible=\"true\"", &format!("visible=\"{visible}\""))
                ),
                Ok(NormalLockScreen {
                    size: (400.0, 800.0),
                    proud_visible: visible
                })
            );
        }
        let with = |extra| {
            NORMAL.replace(
                "</XCUIElementTypeApplication>",
                &format!("{extra}</XCUIElementTypeApplication>"),
            )
        };
        for xml in [
            keypad(),
            with("<XCUIElementTypeAlert/>"),
            with("<XCUIElementTypeSheet/>"),
            with("<XCUIElementTypeSecureTextField value=\"0 of 6 digits\"/>"),
            with("<XCUIElementTypeSecureTextField value=\"1 of 6 digits\"/>"),
            with("<XCUIElementTypeKey/>"),
            with("<XCUIElementTypeButton name=\"1\"/>"),
            with("<XCUIElementTypeOther name=\"passcode-view\"/>"),
            with("<XCUIElementTypeStaticText label=\"Trust This Computer?\"/>"),
            with("<XCUIElementTypeOther name=\"proud-lock\" visible=\"true\"/>"),
            with("<XCUIElementTypeApplication name=\"SpringBoard\"/>"),
            NORMAL.replace("proud-lock", "unknown"),
            NORMAL.replace("visible=\"true\"", ""),
            NORMAL.replace("SpringBoard", "OtherApp"),
            NORMAL.replace("width=\"400\"", "width=\"NaN\""),
            NORMAL.replace("</AppiumAUT>", ""),
            format!("{NORMAL}<extra/>"),
            with("<![CDATA[private]]>"),
            with("&amp;"),
            format!("<!DOCTYPE private>{NORMAL}"),
            with("<a></b>"),
            with("<node name=\"a\" name=\"b\"/>"),
        ] {
            assert_eq!(normal_lock_screen(&xml), Err(Refusal::Screen));
            for visible in [true, false] {
                for index in [0, 1] {
                    let mut fake = Fake::new();
                    fake.captures[index]
                        .as_mut()
                        .unwrap()
                        .raw
                        .accessibility_tree =
                        xml.replace("visible=\"true\"", &format!("visible=\"{visible}\""));
                    assert_eq!(run(&mut fake, &id()), Err(Error::Refused(Refusal::Screen)));
                    assert!(fake.posts.is_empty());
                    assert_eq!(fake.captures_called, index + 1);
                }
            }
        }
    }

    #[test]
    fn every_post_dispatch_failure_is_unknown_with_no_retry() {
        type Change = fn(&mut Snapshot);
        for (reason, change) in [
            (
                UnknownReason::PostconditionUnproven,
                (|s: &mut Snapshot| s.raw.accessibility_tree = NORMAL.into()) as Change,
            ),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.accessibility_tree = keypad().replace("0 of 6 digits", "1 of 6 digits")
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.accessibility_tree = keypad().replace("0 of 6 digits", "0 of 4 digits")
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.accessibility_tree = keypad().replace("name=\"1\"", "name=\"missing\"")
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.accessibility_tree =
                    keypad().replace("</AppiumAUT>", "<XCUIElementTypeAlert/></AppiumAUT>")
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.native.state = LockState::Unlocked
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.native.unlocked_since_boot = None
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.wda_locked = Some(false)
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.wda_target = HardwareId::new("other").unwrap()
            }),
            (UnknownReason::SessionChanged, |s| {
                s.session = "replacement".into()
            }),
            (UnknownReason::PostOrderUnproven, |s| {
                s.started -= Duration::from_secs(2);
                s.fresh_until = s.started + BUDGET;
            }),
            (UnknownReason::PostFreshnessUnproven, |s| {
                s.fresh_until = s.finished
            }),
            (UnknownReason::PostconditionUnproven, |s| {
                s.raw.screenshot.bytes.clear()
            }),
        ] {
            let mut fake = Fake::new();
            change(fake.captures[2].as_mut().unwrap());
            assert_eq!(run(&mut fake, &id()), Err(Error::Unknown(reason)));
            assert_eq!(fake.posts.len(), 1);
            assert_eq!(fake.captures_called, 3);
        }
        let mut fake = Fake::new();
        fake.captures[2] = Err(Refusal::Capture);
        assert_eq!(
            run(&mut fake, &id()),
            Err(Error::Unknown(UnknownReason::PostcaptureFailed))
        );
        assert_eq!((fake.posts.len(), fake.captures_called), (1, 3));
    }

    #[test]
    fn receipt_failure_has_priority_after_one_postcapture_attempt() {
        for (capture_failure, wrong_screen) in [(false, false), (true, false), (false, true)] {
            let mut fake = Fake::new();
            fake.receipt = Err(());
            if capture_failure {
                fake.captures[2] = Err(Refusal::Capture);
            } else if wrong_screen {
                fake.captures[2].as_mut().unwrap().raw.accessibility_tree = NORMAL.into();
            }
            assert_eq!(
                run(&mut fake, &id()),
                Err(Error::Unknown(UnknownReason::ReceiptUnproven))
            );
            assert_eq!(fake.posts.len(), 1);
            assert_eq!(fake.captures_called, 3);
        }
    }

    struct Wire {
        requests: RefCell<Vec<(String, Vec<u8>, Instant)>>,
        result: Result<(u16, &'static [u8]), WdaError>,
    }

    impl HidTransport for Wire {
        async fn post(
            &self,
            path: &str,
            body: &[u8],
            fresh_until: Instant,
        ) -> Result<HttpResponse, WdaError> {
            self.requests
                .borrow_mut()
                .push((path.into(), body.into(), fresh_until));
            self.result.map(|(status, body)| HttpResponse {
                status,
                body: body.into(),
            })
        }
    }

    #[tokio::test]
    async fn fixed_return_endpoint_payload_and_deadline_are_sent_at_most_once() {
        for result in [
            Ok((200, br#"{"value":null}"#.as_slice())),
            Ok((500, b"private failure")),
            Ok((200, b"invalid")),
            Ok((200, br#"{"value":{"error":"private failure"}}"#)),
            Err(WdaError::ExpiredBeforeSend),
            Err(WdaError::AfterSent),
            Err(WdaError::TimedOutAfterSent),
        ] {
            let wire = Wire {
                requests: RefCell::default(),
                result,
            };
            let expires = Instant::now() + BUDGET;
            let receipt = send_return(&wire, "fixture-session", expires).await;
            assert_eq!(
                receipt.is_ok(),
                result == Ok((200, br#"{"value":null}"#.as_slice()))
            );
            assert_eq!(
                *wire.requests.borrow(),
                [(
                    "/session/fixture-session/wda/performIoHidEvent".into(),
                    br#"{"page":7,"usage":40,"duration":0.005}"#.to_vec(),
                    expires
                )]
            );
            let mut fake = Fake::new();
            fake.receipt = receipt;
            assert_eq!(
                run(&mut fake, &id()),
                if receipt.is_ok() {
                    Ok(())
                } else {
                    Err(Error::Unknown(UnknownReason::ReceiptUnproven))
                }
            );
            assert_eq!((fake.posts.len(), fake.captures_called), (1, 3));
        }
        let wire = Wire {
            requests: RefCell::default(),
            result: Ok((200, br#"{"value":null}"#)),
        };
        assert!(
            send_return(&wire, "../invalid", Instant::now())
                .await
                .is_err()
        );
        assert!(wire.requests.borrow().is_empty());
    }

    #[test]
    fn arguments_and_errors_are_fixed_and_private() {
        assert_eq!(command(&[]), Err("invalid_arguments"));
        assert_eq!(
            command(&["--other".into(), "private".into()]),
            Err("invalid_arguments")
        );
        assert_eq!(
            command(&["--config".into(), ".".into()]),
            Err("configuration_invalid")
        );
        for (reason, token) in [
            (Refusal::Identity, "identity_unproven"),
            (Refusal::Session, "session_unproven"),
            (Refusal::NativeLock, "native_lock_unproven"),
            (Refusal::FirstUnlock, "first_unlock_unproven"),
            (Refusal::WdaLock, "wda_lock_unproven"),
            (Refusal::Screen, "screen_unproven"),
            (Refusal::Geometry, "geometry_unproven"),
            (Refusal::Freshness, "freshness_unproven"),
            (Refusal::Capture, "capture_failed"),
        ] {
            assert_eq!(
                format!("reveal_keypad={}", Error::Refused(reason).message()),
                format!("reveal_keypad=refused reason={token}")
            );
        }
        for (reason, token) in [
            (UnknownReason::ReceiptUnproven, "receipt_unproven"),
            (UnknownReason::PostcaptureFailed, "postcapture_failed"),
            (
                UnknownReason::PostFreshnessUnproven,
                "post_freshness_unproven",
            ),
            (
                UnknownReason::PostconditionUnproven,
                "postcondition_unproven",
            ),
            (UnknownReason::SessionChanged, "session_changed"),
            (UnknownReason::PostOrderUnproven, "post_order_unproven"),
        ] {
            assert_eq!(
                format!("reveal_keypad={}", Error::Unknown(reason).message()),
                format!("reveal_keypad=unknown reason={token}")
            );
        }
    }
}
