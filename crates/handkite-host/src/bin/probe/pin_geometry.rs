use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{HardwareId, LockState};
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_settings::validated_active_session;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, native_lock, pin_screen};
use quick_xml::events::Event;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(5);

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("refused reason=invalid_arguments");
    };
    if flag != "--config" {
        return Err("refused reason=invalid_arguments");
    }
    let binding =
        host_config::load(Path::new(path)).map_err(|_| "refused reason=configuration_invalid")?;
    let mut live = Live::new(binding.usb_id.clone()).map_err(Refusal::message)?;
    let summary = inspect(&mut live, &binding.usb_id).map_err(Refusal::message)?;
    println!("{}", summary.line());
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    Identity,
    Session,
    Lock,
    Capture,
    Freshness,
    Geometry,
}

impl Refusal {
    fn message(self) -> &'static str {
        match self {
            Self::Identity => "refused reason=identity_unproven",
            Self::Session => "refused reason=session_unproven",
            Self::Lock => "refused reason=lock_unproven",
            Self::Capture => "refused reason=capture_failed",
            Self::Freshness => "refused reason=freshness_unproven",
            Self::Geometry => "refused reason=geometry_unproven",
        }
    }
}

struct Status {
    usb: HardwareId,
    target: HardwareId,
    session: String,
    native_locked: bool,
    wda_locked: bool,
    springboard: bool,
}

struct Capture {
    raw: RawObservation,
    started: Instant,
    finished: Instant,
}

// There is no input or session-creation operation in this interface.
trait ReadDevice {
    fn status(&mut self) -> Result<Status, Refusal>;
    fn capture(&mut self) -> Result<Capture, Refusal>;
    fn now(&self) -> Instant;
}

fn inspect(device: &mut impl ReadDevice, expected: &HardwareId) -> Result<Summary, Refusal> {
    let before = device.status()?;
    check(&before, expected)?;
    let capture = device.capture()?;
    let after = device.status()?;
    check(&after, expected)?;
    if before.session != after.session {
        return Err(Refusal::Session);
    }
    let raw = &capture.raw;
    if raw.connected_usb != *expected || raw.wda_target != *expected {
        return Err(Refusal::Identity);
    }
    if !raw.state.wda_ready {
        return Err(Refusal::Session);
    }
    if raw.state.lock != LockState::Locked
        || raw.state.foreground_app.as_deref() != Some("com.apple.springboard")
    {
        return Err(Refusal::Lock);
    }
    let now = device.now();
    if capture.started > capture.finished
        || capture.finished > now
        || now.duration_since(capture.started) >= BUDGET
        || raw.window.duration_ms().map_err(|_| Refusal::Freshness)? > BUDGET.as_millis() as u64
    {
        return Err(Refusal::Freshness);
    }
    geometry(&raw.accessibility_tree, &raw.screenshot.bytes)
}

fn check(status: &Status, expected: &HardwareId) -> Result<(), Refusal> {
    if status.usb != *expected || status.target != *expected {
        return Err(Refusal::Identity);
    }
    if status.session.is_empty()
        || status.session.len() > 128
        || !status
            .session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Refusal::Session);
    }
    if !status.native_locked || !status.wda_locked || !status.springboard {
        return Err(Refusal::Lock);
    }
    Ok(())
}

struct Summary {
    screen: (u16, u16),
    png: (u32, u32),
    centers: [(u16, u16); 10],
}

impl Summary {
    fn line(&self) -> String {
        let mut output = format!(
            "pin_geometry=observed screen={}x{} png={}x{} centers_permille=",
            self.screen.0, self.screen.1, self.png.0, self.png.1
        );
        for (digit, (x, y)) in self.centers.iter().enumerate() {
            if digit != 0 {
                output.push(';');
            }
            output.push_str(&format!("{digit}:{x},{y}"));
        }
        output
    }
}

fn geometry(xml: &str, png: &[u8]) -> Result<Summary, Refusal> {
    // Reuse the strict in-Application, no-alert, unique-field/key, empty-six-digit proof.
    if !super::lock_ui_attrs::empty_keypad(xml) {
        return Err(Refusal::Geometry);
    }
    let view = pin_screen::parse(xml).map_err(|_| Refusal::Geometry)?;
    let (width, height) = view.screen_size.ok_or(Refusal::Geometry)?;
    if width.fract() != 0.0 || height.fract() != 0.0 {
        return Err(Refusal::Geometry);
    }
    origin_and_key_count(xml)?;
    let keys = view.keypad.ok_or(Refusal::Geometry)?;
    if png.len() < 24
        || !png.starts_with(b"\x89PNG\r\n\x1a\n")
        || png[8..12] != 13u32.to_be_bytes()
        || &png[12..16] != b"IHDR"
    {
        return Err(Refusal::Capture);
    }
    let image_width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| Refusal::Capture)?);
    let image_height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| Refusal::Capture)?);
    if !(1..=6000).contains(&image_width)
        || !(1..=6000).contains(&image_height)
        || (f64::from(image_width) / f64::from(image_height) - width / height).abs() > 0.02
    {
        return Err(Refusal::Geometry);
    }
    let centers = keys.map(|key| {
        (
            (key.x / width * 1000.0).round() as u16,
            (key.y / height * 1000.0).round() as u16,
        )
    });
    Ok(Summary {
        screen: (width as u16, height as u16),
        png: (image_width, image_height),
        centers,
    })
}

fn origin_and_key_count(xml: &str) -> Result<(), Refusal> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut keys = 0;
    loop {
        match reader.read_event().map_err(|_| Refusal::Geometry)? {
            Event::Start(node) | Event::Empty(node) => {
                if node.name().as_ref() == b"XCUIElementTypeKey" {
                    keys += 1;
                }
                if node.name().as_ref() == b"XCUIElementTypeApplication" {
                    let (mut x, mut y) = (None, None);
                    for attr in node.attributes() {
                        let attr = attr.map_err(|_| Refusal::Geometry)?;
                        let value = attr
                            .decode_and_unescape_value(reader.decoder())
                            .map_err(|_| Refusal::Geometry)?;
                        match attr.key.as_ref() {
                            b"x" => x = value.parse::<f64>().ok(),
                            b"y" => y = value.parse::<f64>().ok(),
                            _ => {}
                        }
                    }
                    if x != Some(0.0) || y != Some(0.0) {
                        return Err(Refusal::Geometry);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if keys != 10 {
        return Err(Refusal::Geometry);
    }
    Ok(())
}

struct Live {
    expected: HardwareId,
    adapter: WdaAdapter,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
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
        })
    }

    fn get(&self, path: &str) -> Result<Value, Refusal> {
        let response = self
            .runtime
            .block_on(self.wda.request("GET", path, b"", Duration::from_secs(10)))
            .map_err(|_| Refusal::Capture)?;
        let body: Value = serde_json::from_slice(&response.body).map_err(|_| Refusal::Capture)?;
        if response.status != 200
            || body.get("value").is_none()
            || body["value"].get("error").is_some()
        {
            return Err(Refusal::Capture);
        }
        Ok(body["value"].clone())
    }
}

impl ReadDevice for Live {
    fn status(&mut self) -> Result<Status, Refusal> {
        let (usb, target) = self.adapter.identities().map_err(|_| Refusal::Identity)?;
        let native = native_lock::query(&self.expected);
        let session = self
            .runtime
            .block_on(validated_active_session(&self.wda))
            .map_err(|_| Refusal::Session)?;
        Ok(Status {
            usb,
            target,
            session,
            native_locked: native.state == LockState::Locked
                && native.unlocked_since_boot == Some(true),
            wda_locked: self.get("/wda/locked")?.as_bool() == Some(true),
            springboard: self.get("/wda/activeAppInfo")?["bundleId"].as_str()
                == Some("com.apple.springboard"),
        })
    }

    fn capture(&mut self) -> Result<Capture, Refusal> {
        let started = Instant::now();
        let (raw, finished) = self
            .adapter
            .observe_pin_with_attributes()
            .map_err(|_| Refusal::Capture)?;
        Ok(Capture {
            raw,
            started,
            finished,
        })
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};
    use std::collections::VecDeque;

    const FIELD: &str = r#"<XCUIElementTypeSecureTextField visible="true" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/>"#;
    const LINE: &str = "pin_geometry=observed screen=400x800 png=1200x2400 centers_permille=0:450,800;1:200,425;2:450,425;3:700,425;4:200,550;5:450,550;6:700,550;7:200,675;8:450,675;9:700,675";

    fn id() -> HardwareId {
        HardwareId::new("fixture-phone").unwrap()
    }

    fn tree() -> String {
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
        xml.push_str(r#"<XCUIElementTypeStaticText label="not-for-output"/></XCUIElementTypeApplication></AppiumAUT>"#);
        xml
    }

    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        bytes.extend_from_slice(&1200u32.to_be_bytes());
        bytes.extend_from_slice(&2400u32.to_be_bytes());
        bytes
    }

    fn status() -> Status {
        Status {
            usb: id(),
            target: id(),
            session: "fixture-session".into(),
            native_locked: true,
            wda_locked: true,
            springboard: true,
        }
    }

    struct Fake {
        statuses: VecDeque<Result<Status, Refusal>>,
        captured: Option<Result<Capture, Refusal>>,
        now: Instant,
        captures: usize,
    }

    impl Fake {
        fn new() -> Self {
            let started = Instant::now();
            Self {
                statuses: [Ok(status()), Ok(status())].into(),
                captured: Some(Ok(Capture {
                    raw: RawObservation {
                        connected_usb: id(),
                        wda_target: id(),
                        window: ObservationWindow {
                            started_unix_ms: 100,
                            finished_unix_ms: 101,
                        },
                        state: DeviceState {
                            lock: LockState::Locked,
                            wda_ready: true,
                            foreground_app: Some("com.apple.springboard".into()),
                        },
                        accessibility_tree: tree(),
                        screenshot: ScreenImage {
                            format: ImageFormat::Png,
                            bytes: png(),
                        },
                    },
                    started,
                    finished: started + Duration::from_millis(1),
                })),
                now: started + Duration::from_millis(2),
                captures: 0,
            }
        }
    }

    impl ReadDevice for Fake {
        fn status(&mut self) -> Result<Status, Refusal> {
            self.statuses.pop_front().expect("extra status")
        }
        fn capture(&mut self) -> Result<Capture, Refusal> {
            self.captures += 1;
            self.captured.take().expect("capture replay")
        }
        fn now(&self) -> Instant {
            self.now
        }
    }

    #[test]
    fn one_paired_capture_reports_only_bounded_dimensions_and_normalized_centers() {
        for hidden in [false, true] {
            let mut fake = Fake::new();
            if hidden {
                let capture = fake.captured.as_mut().unwrap().as_mut().unwrap();
                capture.raw.accessibility_tree =
                    tree().replace("visible=\"true\"", "visible=\"false\"");
            }
            let line = inspect(&mut fake, &id()).unwrap().line();
            assert_eq!(line, LINE);
            for private in [
                "fixture-phone",
                "fixture-session",
                "not-for-output",
                "0 of 6 digits",
                "<",
                "value=",
            ] {
                assert!(!line.contains(private));
            }
            assert_eq!(fake.captures, 1);
            assert!(fake.statuses.is_empty());
        }
    }

    #[test]
    fn identity_session_and_lock_fail_closed_before_and_after_capture() {
        type Change = fn(&mut Status);
        for (reason, change) in [
            (
                Refusal::Identity,
                (|s: &mut Status| s.usb = HardwareId::new("other").unwrap()) as Change,
            ),
            (Refusal::Identity, |s| {
                s.target = HardwareId::new("other").unwrap()
            }),
            (Refusal::Session, |s| s.session = "../invalid".into()),
            (Refusal::Lock, |s| s.native_locked = false),
            (Refusal::Lock, |s| s.wda_locked = false),
            (Refusal::Lock, |s| s.springboard = false),
        ] {
            for index in 0..2 {
                let mut fake = Fake::new();
                change(fake.statuses[index].as_mut().unwrap());
                assert!(matches!(inspect(&mut fake, &id()), Err(actual) if actual == reason));
                assert_eq!(fake.captures, index);
            }
        }
        let mut fake = Fake::new();
        fake.statuses[1].as_mut().unwrap().session = "replacement-session".into();
        assert!(matches!(inspect(&mut fake, &id()), Err(Refusal::Session)));
        assert_eq!(fake.captures, 1);
    }

    #[test]
    fn stale_mismatched_or_failed_capture_never_returns_geometry() {
        type Change = fn(&mut Capture);
        for (reason, change) in [
            (
                Refusal::Identity,
                (|c: &mut Capture| c.raw.wda_target = HardwareId::new("other").unwrap()) as Change,
            ),
            (Refusal::Session, |c| c.raw.state.wda_ready = false),
            (Refusal::Lock, |c| c.raw.state.lock = LockState::Unlocked),
            (Refusal::Freshness, |c| c.started -= BUDGET),
            (Refusal::Freshness, |c| c.finished += BUDGET),
            (Refusal::Freshness, |c| c.raw.window.finished_unix_ms = 99),
        ] {
            let mut fake = Fake::new();
            change(fake.captured.as_mut().unwrap().as_mut().unwrap());
            assert!(matches!(inspect(&mut fake, &id()), Err(actual) if actual == reason));
            assert_eq!(fake.captures, 1);
        }
        let mut fake = Fake::new();
        fake.now = fake.captured.as_ref().unwrap().as_ref().unwrap().started + BUDGET;
        assert!(matches!(inspect(&mut fake, &id()), Err(Refusal::Freshness)));
        let mut fake = Fake::new();
        fake.captured = Some(Err(Refusal::Capture));
        assert!(matches!(inspect(&mut fake, &id()), Err(Refusal::Capture)));
        assert_eq!(fake.captures, 1);
    }

    #[test]
    fn missing_duplicate_malformed_or_ambiguous_keypad_fields_refuse() {
        let xml = tree();
        for invalid in [
            xml.replace("name=\"9\"", "name=\"missing\""),
            xml.replace(FIELD, ""),
            xml.replace(FIELD, &format!("{FIELD}{FIELD}")),
            xml.replace("name=\"9\"", "name=\"1\""),
            xml.replace("0 of 6 digits", "not-for-output"),
            xml.replace("0 of 6 digits", "1 of 6 digits"),
            xml.replace("visible=\"true\"", "visible=\"invalid\""),
            xml.replace("width=\"400\"", "width=\"400\" width=\"401\""),
            xml.replace("x=\"0\" y=\"0\"", "x=\"20\" y=\"0\""),
            xml.replace("x=\"0\" y=\"0\"", "y=\"0\""),
            xml.replace("width=\"80\"", "width=\"NaN\""),
            xml.replace("</AppiumAUT>", "<XCUIElementTypeApplication/></AppiumAUT>"),
            xml.replace("</AppiumAUT>", "<XCUIElementTypeAlert/></AppiumAUT>"),
            xml.replace("</AppiumAUT>", ""),
        ] {
            assert!(geometry(&invalid, &png()).is_err());
        }
        for bytes in [
            vec![],
            b"not-for-output".to_vec(),
            {
                let mut image = png();
                image[16..20].copy_from_slice(&6001u32.to_be_bytes());
                image
            },
            {
                let mut image = png();
                image[20..24].copy_from_slice(&1200u32.to_be_bytes());
                image
            },
        ] {
            assert!(geometry(&xml, &bytes).is_err());
        }
    }

    #[test]
    fn arguments_and_refusal_tokens_are_fixed() {
        assert_eq!(command(&[]), Err("refused reason=invalid_arguments"));
        assert_eq!(
            command(&["--other".into(), "private".into()]),
            Err("refused reason=invalid_arguments")
        );
        for (reason, token) in [
            (Refusal::Identity, "identity_unproven"),
            (Refusal::Session, "session_unproven"),
            (Refusal::Lock, "lock_unproven"),
            (Refusal::Capture, "capture_failed"),
            (Refusal::Freshness, "freshness_unproven"),
            (Refusal::Geometry, "geometry_unproven"),
        ] {
            assert_eq!(reason.message(), format!("refused reason={token}"));
        }
    }
}
