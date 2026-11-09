use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{Action, HardwareId, LockState, Point};
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_action::WdaActions;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, native_lock};
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLOCK: &str = "com.apple.mobiletimer";
const MAX_CAPTURE_MS: u64 = 10_000;
const MAX_AGE_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Transport {
    W3c,
    Coordinate,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Self::W3c => "w3c",
            Self::Coordinate => "coordinate",
        }
    }
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
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
    let binding = host_config::load(Path::new(config)).map_err(|_| "configuration_invalid")?;
    let mut live = Live::new(binding.usb_id.clone()).map_err(|_| "refused")?;
    run(&mut live, &binding.usb_id, transport).map_err(|error| match error {
        TrialError::Refused => "refused",
        TrialError::Unknown => "unknown",
    })?;
    println!("touch_trial=confirmed transport={}", transport.name());
    Ok(())
}

struct Snapshot {
    observation: RawObservation,
    session: String,
    native: LockState,
    fresh_until: Instant,
}

trait TrialDevice {
    fn capture(&mut self) -> Result<Snapshot, ()>;
    fn now_ms(&self) -> Result<u64, ()>;
    fn tap(&mut self, transport: Transport, at: Point, fresh_until: Instant) -> Result<(), ()>;
}

#[derive(Debug, PartialEq)]
enum TrialError {
    Refused,
    Unknown,
}

fn run(
    device: &mut impl TrialDevice,
    expected: &HardwareId,
    transport: Transport,
) -> Result<(), TrialError> {
    let before = device.capture().map_err(|_| TrialError::Refused)?;
    let first =
        validate(&before, expected, device.now_ms(), false).map_err(|_| TrialError::Refused)?;
    // Reobserve every precondition; only stable accessibility geometry authorizes input.
    let current = device.capture().map_err(|_| TrialError::Refused)?;
    let second =
        validate(&current, expected, device.now_ms(), false).map_err(|_| TrialError::Refused)?;
    if current.session != before.session
        || current.observation.window.started_unix_ms < before.observation.window.finished_unix_ms
        || first != second
    {
        return Err(TrialError::Refused);
    }
    let input_started = device.now_ms().map_err(|_| TrialError::Refused)?;
    let receipt = device.tap(transport, second.center(), current.fresh_until);
    // Even unknown delivery receives one read-only postcheck, never another input.
    let after = device.capture().map_err(|_| TrialError::Unknown)?;
    validate(&after, expected, device.now_ms(), true).map_err(|_| TrialError::Unknown)?;
    if after.session != current.session
        || after.observation.window.started_unix_ms < input_started
        || receipt.is_err()
    {
        return Err(TrialError::Unknown);
    }
    Ok(())
}

fn validate(
    snapshot: &Snapshot,
    expected: &HardwareId,
    now: Result<u64, ()>,
    after: bool,
) -> Result<Rect, ()> {
    let raw = &snapshot.observation;
    if Instant::now() >= snapshot.fresh_until
        || raw.connected_usb != *expected
        || raw.wda_target != *expected
        || !valid_session(&snapshot.session)
        || snapshot.native != LockState::Unlocked
        || raw.state.lock != LockState::Unlocked
        || !raw.state.wda_ready
        || raw.state.foreground_app.as_deref() != Some(CLOCK)
        || raw.window.duration_ms().map_err(|_| ())? > MAX_CAPTURE_MS
        || now?.checked_sub(raw.window.finished_unix_ms).ok_or(())? > MAX_AGE_MS
    {
        return Err(());
    }
    let png = &raw.screenshot.bytes;
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || &png[12..16] != b"IHDR" {
        return Err(());
    }
    let width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| ())?);
    let height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| ())?);
    let evidence = parse(&raw.accessibility_tree)?;
    // Coordinates come from accessibility points; image pixels only check screen shape.
    if width == 0
        || height == 0
        || (f64::from(width) / f64::from(height) - evidence.screen.w / evidence.screen.h).abs()
            > 0.02
        || evidence.stopwatch.selected != Some(after)
        || evidence.world.selected != Some(!after)
        || (after && !evidence.start_visible)
    {
        return Err(());
    }
    Ok(evidence.stopwatch.rect)
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

struct Tab {
    rect: Rect,
    visible: Option<bool>,
    enabled: Option<bool>,
    selected: Option<bool>,
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
        })
    }

    fn eligible(&self) -> bool {
        self.visible == Some(true) && self.enabled == Some(true) && self.selected.is_some()
    }
}

struct Evidence {
    screen: Rect,
    stopwatch: Tab,
    world: Tab,
    start_visible: bool,
}

fn parse(xml: &str) -> Result<Evidence, ()> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(());
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut screen = None;
    let mut tabbar = None;
    let mut stopwatch = None;
    let mut world = None;
    let mut start_visible = false;
    let mut start_seen = false;
    loop {
        let event = reader.read_event().map_err(|_| ())?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(ref node) | Event::Empty(ref node) => {
                let kind = String::from_utf8(node.name().as_ref().to_vec()).map_err(|_| ())?;
                if stack.len() > 128
                    || matches!(
                        kind.as_str(),
                        "XCUIElementTypeAlert"
                            | "XCUIElementTypeSheet"
                            | "XCUIElementTypeSecureTextField"
                    )
                {
                    return Err(());
                }
                let mut attrs = BTreeMap::new();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| ())?;
                    let key = String::from_utf8(attr.key.as_ref().to_vec()).map_err(|_| ())?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| ())?;
                    if [
                        "proud-lock",
                        "passcode-view",
                        "call-to-action-label",
                        "Enter Passcode",
                    ]
                    .iter()
                    .any(|marker| value.contains(marker))
                    {
                        return Err(());
                    }
                    attrs.insert(key, value.into_owned());
                }
                if kind == "XCUIElementTypeApplication" {
                    if screen.is_some() {
                        return Err(());
                    }
                    let rect = Rect::read(&attrs)?;
                    if rect.x != 0.0 || rect.y != 0.0 {
                        return Err(());
                    }
                    screen = Some(rect);
                }
                if kind == "XCUIElementTypeTabBar" {
                    if tabbar.is_some() || !stack.iter().any(|s| s == "XCUIElementTypeApplication")
                    {
                        return Err(());
                    }
                    tabbar = Some(Rect::read(&attrs)?);
                }
                let name = attrs.get("name").map(String::as_str);
                if matches!(name, Some("Stopwatch" | "World Clock"))
                    && kind == "XCUIElementTypeButton"
                {
                    if !stack.iter().any(|s| s == "XCUIElementTypeTabBar") {
                        return Err(());
                    }
                    let slot = if name == Some("Stopwatch") {
                        &mut stopwatch
                    } else {
                        &mut world
                    };
                    if slot.is_some() {
                        return Err(());
                    }
                    *slot = Some(Tab::read(&attrs)?);
                }
                if name == Some("Start")
                    && kind == "XCUIElementTypeButton"
                    && !stack.iter().any(|s| s == "XCUIElementTypeTabBar")
                {
                    if start_seen || !stack.iter().any(|s| s == "XCUIElementTypeApplication") {
                        return Err(());
                    }
                    start_seen = true;
                    let button = Tab::read(&attrs)?;
                    start_visible = button.visible == Some(true)
                        && button.enabled == Some(true)
                        && screen.ok_or(())?.contains(button.rect);
                }
                if is_start {
                    stack.push(kind);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(())?;
            }
            Event::Eof => break,
            Event::DocType(_) => return Err(()),
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(());
    }
    let evidence = Evidence {
        screen: screen.ok_or(())?,
        stopwatch: stopwatch.ok_or(())?,
        world: world.ok_or(())?,
        start_visible,
    };
    let bar = tabbar.ok_or(())?;
    if !evidence.stopwatch.eligible()
        || !evidence.world.eligible()
        || !evidence.screen.contains(bar)
        || !bar.contains(evidence.stopwatch.rect)
        || !bar.contains(evidence.world.rect)
        || evidence.stopwatch.rect.overlaps(evidence.world.rect)
    {
        return Err(());
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
    fn new(expected: HardwareId) -> Result<Self, ()> {
        Ok(Self {
            adapter: WdaAdapter::new(expected.clone()).map_err(|_| ())?,
            wda: UsbWda::new(PinnedUsb::new(expected.clone())),
            expected,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ())?,
            actions: WdaActions::default(),
        })
    }

    fn status(&self) -> Result<String, ()> {
        let response = self
            .runtime
            .block_on(
                self.wda
                    .request("GET", "/status", b"", Duration::from_secs(10)),
            )
            .map_err(|_| ())?;
        let json: serde_json::Value = serde_json::from_slice(&response.body).map_err(|_| ())?;
        let session = json["sessionId"]
            .as_str()
            .filter(|id| valid_session(id))
            .ok_or(())?;
        if response.status != 200
            || json["value"]["ready"] != true
            || json["value"].get("error").is_some()
        {
            return Err(());
        }
        Ok(session.to_owned())
    }
}

fn valid_session(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl TrialDevice for Live {
    fn capture(&mut self) -> Result<Snapshot, ()> {
        let started = self.now_ms()?;
        self.adapter.identities().map_err(|_| ())?;
        let session = self.status()?;
        if native_lock::query(&self.expected).state != LockState::Unlocked {
            return Err(());
        }
        let (mut observation, captured_at) =
            self.adapter.observe_with_attributes().map_err(|_| ())?;
        let fresh_until = captured_at
            .checked_add(Duration::from_millis(MAX_AGE_MS))
            .ok_or(())?;
        self.runtime
            .block_on(self.actions.prepare_existing(&self.wda))
            .map_err(|_| ())?;
        if !self.actions.prepared_session_matches(&session) || self.status()? != session {
            return Err(());
        }
        let native = native_lock::query(&self.expected).state;
        self.adapter.identities().map_err(|_| ())?;
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

    fn tap(&mut self, transport: Transport, at: Point, fresh_until: Instant) -> Result<(), ()> {
        let wda = self.wda.with_write_deadline(fresh_until);
        let receipt = match transport {
            Transport::W3c => self
                .runtime
                .block_on(self.actions.execute_existing(&wda, &Action::Tap { at })),
            Transport::Coordinate => self.runtime.block_on(self.actions.tap_existing(&wda, at)),
        }
        .map_err(|_| ())?;
        if receipt.accepted == Some(true) {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use std::collections::VecDeque;

    fn tree(after: bool) -> String {
        format!(
            r#"<AppiumAUT><XCUIElementTypeApplication name="Clock" x="0" y="0" width="400" height="800">
          <XCUIElementTypeButton name="Start" visible="{after}" enabled="true" x="300" y="500" width="60" height="60"/>
          <XCUIElementTypeTabBar x="0" y="700" width="400" height="100">
            <XCUIElementTypeButton name="World Clock" visible="true" enabled="true" selected="{}" x="0" y="700" width="100" height="80"/>
            <XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="{after}" x="200" y="700" width="100" height="80"/>
          </XCUIElementTypeTabBar>
        </XCUIElementTypeApplication></AppiumAUT>"#,
            !after
        )
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
        captures: VecDeque<Result<Snapshot, ()>>,
        taps: Vec<(Transport, Point, Instant)>,
        receipt: Result<(), ()>,
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
        fn capture(&mut self) -> Result<Snapshot, ()> {
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
        fn tap(&mut self, transport: Transport, at: Point, fresh_until: Instant) -> Result<(), ()> {
            assert!(fresh_until > Instant::now());
            assert!(self.taps.is_empty(), "second input or transport fallback");
            self.taps.push((transport, at, fresh_until));
            self.receipt
        }
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
    fn both_preflight_captures_reject_stale_mismatched_locked_and_unready_evidence() {
        let mutations: &[fn(&mut Snapshot)] = &[
            |s| s.fresh_until = Instant::now(),
            |s| s.observation.connected_usb = HardwareId::new("other-device").unwrap(),
            |s| s.observation.wda_target = HardwareId::new("other-device").unwrap(),
            |s| s.native = LockState::Locked,
            |s| s.native = LockState::Uncertain,
            |s| s.observation.state.lock = LockState::Locked,
            |s| s.observation.state.lock = LockState::Uncertain,
            |s| s.observation.state.wda_ready = false,
            |s| s.observation.state.foreground_app = Some("com.apple.springboard".into()),
            |s| s.session = "../invalid".into(),
            |s| s.observation.window.finished_unix_ms = 0,
            |s| s.observation.window.finished_unix_ms = 20_000,
            |s| s.observation.screenshot.bytes.clear(),
            |s| s.observation.accessibility_tree = tree(true),
        ];
        for position in 0..2 {
            for mutate in mutations {
                let mut fixture = Fixture::new();
                mutate(fixture.captures[position].as_mut().unwrap());
                assert_eq!(fixture.run(Transport::W3c), Err(TrialError::Refused));
                assert!(fixture.taps.is_empty());
            }
        }
        let mut fixture = Fixture::new();
        fixture.age = 4000;
        assert_eq!(fixture.run(Transport::Coordinate), Err(TrialError::Refused));
        assert!(fixture.taps.is_empty());
    }

    #[test]
    fn revalidation_refuses_changed_session_geometry_or_reused_capture() {
        let mutations: &[fn(&mut Snapshot)] = &[
            |s| s.session = "replacement-session".into(),
            |s| s.observation.accessibility_tree = tree(false).replace("x=\"200\"", "x=\"210\""),
            |s| s.observation.window.started_unix_ms = 100,
        ];
        for mutate in mutations {
            let mut fixture = Fixture::new();
            mutate(fixture.captures[1].as_mut().unwrap());
            assert_eq!(fixture.run(Transport::W3c), Err(TrialError::Refused));
            assert!(fixture.taps.is_empty());
        }
    }

    #[test]
    fn ambiguous_hidden_disabled_missing_attributes_and_unsafe_trees_refuse_input() {
        let original = tree(false);
        let duplicate = r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="false" x="200" y="700" width="100" height="80"/>"#;
        let cases = [
            original.replace(
                "</XCUIElementTypeTabBar>",
                &format!("{duplicate}</XCUIElementTypeTabBar>"),
            ),
            original.replace("visible=\"true\"", "visible=\"false\""),
            original.replace("enabled=\"true\"", "enabled=\"false\""),
            original.replace(" selected=\"false\"", ""),
            original.replace(" visible=\"true\"", ""),
            original.replace(" enabled=\"true\"", ""),
            original.replace("x=\"200\"", "x=\"NaN\""),
            original.replace("x=\"200\"", "x=\"399\""),
            original.replace("</AppiumAUT>", "<XCUIElementTypeAlert/></AppiumAUT>"),
            original.replace(
                "</AppiumAUT>",
                "<XCUIElementTypeSecureTextField/></AppiumAUT>",
            ),
            original.replace("</AppiumAUT>", "<node name=\"proud-lock\"/></AppiumAUT>"),
            original.replace("</AppiumAUT>", ""),
        ];
        for xml in cases {
            let mut fixture = Fixture::new();
            fixture.captures[1]
                .as_mut()
                .unwrap()
                .observation
                .accessibility_tree = xml;
            assert_eq!(fixture.run(Transport::Coordinate), Err(TrialError::Refused));
            assert!(fixture.taps.is_empty());
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
        for mutate in mutations {
            let mut fixture = Fixture::new();
            mutate(fixture.captures[2].as_mut().unwrap());
            assert_eq!(fixture.run(Transport::W3c), Err(TrialError::Unknown));
            assert_eq!(fixture.taps.len(), 1);
            assert!(fixture.captures.is_empty());
        }
        let mut fixture = Fixture::new();
        fixture.captures[2] = Err(());
        assert_eq!(fixture.run(Transport::Coordinate), Err(TrialError::Unknown));
        assert_eq!(fixture.taps.len(), 1);
    }

    #[test]
    fn unknown_receipt_still_observes_once_without_fallback() {
        for transport in [Transport::W3c, Transport::Coordinate] {
            let mut fixture = Fixture::new();
            fixture.receipt = Err(());
            assert_eq!(fixture.run(transport), Err(TrialError::Unknown));
            assert_eq!(fixture.taps.len(), 1);
            assert!(fixture.captures.is_empty());
        }
    }

    #[test]
    fn arguments_and_config_fail_before_device_initialization() {
        for args in [
            vec![],
            vec!["--transport", "w3c"],
            vec!["--config", "missing", "--transport", "fallback"],
            vec!["--config", "missing", "--transport", "w3c", "extra"],
        ] {
            assert_eq!(
                command(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()),
                Err("invalid_arguments")
            );
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
