use handkite_core::passcode::PasscodeProgress;
use handkite_core::protocol::{HardwareId, LockState};
use handkite_host::native_lock::NativeLock;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, native_lock, pin_screen};
use quick_xml::events::Event;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const SPRINGBOARD: &str = "com.apple.springboard";
const SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    Identity,
    NativeLock,
    FirstUnlock,
    Session,
    WdaLock,
    Screen,
    Geometry,
    Progress,
    Capture,
}

impl Refusal {
    fn message(self) -> &'static str {
        match self {
            Self::Identity => "refused reason=identity_unproven",
            Self::NativeLock => "refused reason=native_lock_unproven",
            Self::FirstUnlock => "refused reason=first_unlock_unproven",
            Self::Session => "refused reason=session_unproven",
            Self::WdaLock => "refused reason=wda_lock_unproven",
            Self::Screen => "refused reason=screen_unproven",
            Self::Geometry => "refused reason=geometry_unproven",
            Self::Progress => "refused reason=progress_unproven",
            Self::Capture => "refused reason=capture_failed",
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
    let mut live = Live::new(binding.usb_id).map_err(Refusal::message)?;
    let summary = inspect(&mut live).map_err(Refusal::message)?;
    println!("{}", summary.line());
    Ok(())
}

// This interface has no input or session-creation operation.
trait ReadDevice {
    fn identity(&mut self) -> Result<(), Refusal>;
    fn native(&mut self) -> NativeLock;
    fn get(&mut self, path: &str) -> Result<Value, Refusal>;
}

struct Live {
    expected: HardwareId,
    usb: PinnedUsb,
    runtime: tokio::runtime::Runtime,
}

impl Live {
    fn new(expected: HardwareId) -> Result<Self, Refusal> {
        Ok(Self {
            usb: PinnedUsb::new(expected.clone()),
            expected,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Refusal::Capture)?,
        })
    }
}

impl ReadDevice for Live {
    fn identity(&mut self) -> Result<(), Refusal> {
        self.runtime
            .block_on(self.usb.provider())
            .map(|_| ())
            .map_err(|_| Refusal::Identity)
    }

    fn native(&mut self) -> NativeLock {
        native_lock::query(&self.expected)
    }

    fn get(&mut self, path: &str) -> Result<Value, Refusal> {
        let wda = UsbWda::new(self.usb.clone());
        let response = self
            .runtime
            .block_on(wda.request("GET", path, b"", Duration::from_secs(10)))
            .map_err(|_| Refusal::Capture)?;
        response_json(response.status, &response.body)
    }
}

fn response_json(status: u16, body: &[u8]) -> Result<Value, Refusal> {
    let json: Value = serde_json::from_slice(body).map_err(|_| Refusal::Capture)?;
    if status != 200 || json.get("value").is_none() || json["value"].get("error").is_some() {
        return Err(Refusal::Capture);
    }
    Ok(json)
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

fn session(device: &mut impl ReadDevice) -> Result<String, Refusal> {
    let json = device.get("/status").map_err(|_| Refusal::Session)?;
    let id = json["sessionId"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .ok_or(Refusal::Session)?;
    if json["value"]["ready"] != true {
        return Err(Refusal::Session);
    }
    device
        .get(&format!("/session/{id}"))
        .map_err(|_| Refusal::Session)?;
    Ok(id.to_owned())
}

fn wda_locked(device: &mut impl ReadDevice) -> Result<(), Refusal> {
    if device.get("/wda/locked").map_err(|_| Refusal::WdaLock)?["value"] != true {
        return Err(Refusal::WdaLock);
    }
    if device
        .get("/wda/activeAppInfo")
        .map_err(|_| Refusal::Screen)?["value"]["bundleId"]
        != SPRINGBOARD
    {
        return Err(Refusal::Screen);
    }
    Ok(())
}

fn inspect(device: &mut impl ReadDevice) -> Result<Summary, Refusal> {
    inspect_with(device, parse)
}

pub(super) fn with_locked_xml<T>(
    expected: HardwareId,
    parse: impl FnOnce(&str) -> Result<T, ()>,
) -> Result<T, &'static str> {
    let mut live = Live::new(expected).map_err(Refusal::message)?;
    inspect_with(&mut live, |xml| parse(xml).map_err(|_| Refusal::Capture))
        .map_err(Refusal::message)
}

fn inspect_with<T>(
    device: &mut impl ReadDevice,
    parse: impl FnOnce(&str) -> Result<T, Refusal>,
) -> Result<T, Refusal> {
    device.identity()?;
    native_locked(device.native())?;
    let before = session(device)?;
    wda_locked(device)?;
    let source = device.get(SOURCE)?;
    let summary = parse(source["value"].as_str().ok_or(Refusal::Capture)?)?;
    wda_locked(device)?;
    if session(device)? != before {
        return Err(Refusal::Session);
    }
    native_locked(device.native())?;
    device.identity()?;
    Ok(summary)
}

#[derive(Clone, Copy, Default)]
enum Flag {
    True,
    False,
    #[default]
    Missing,
}

impl Flag {
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

#[derive(Clone, Copy, Default, PartialEq)]
enum Token {
    #[default]
    Other,
    Cancel,
    Excluded,
    SpringBoard,
    PasscodeView,
    ProudLock,
    Heading,
    Trust,
}

impl Token {
    fn parse(value: &str) -> Self {
        match value {
            "Cancel" => Self::Cancel,
            "Emergency" | "Delete" => Self::Excluded,
            "SpringBoard" => Self::SpringBoard,
            "passcode-view" => Self::PasscodeView,
            "proud-lock" => Self::ProudLock,
            "Enter Passcode" => Self::Heading,
            "Trust" | "Trust This Computer" | "Trust This Computer?" => Self::Trust,
            _ => Self::Other,
        }
    }
}

#[derive(Default)]
struct Button {
    visible: Flag,
    enabled: Flag,
    rect: [Option<f64>; 4],
}

#[derive(Default)]
struct Summary {
    count: u8,
    visible: Flag,
    enabled: Flag,
    in_screen: bool,
}

impl Summary {
    fn line(&self) -> String {
        let count = match self.count {
            0 => "zero",
            1 => "one",
            _ => "multiple",
        };
        format!(
            "lock_ui_attrs=observed cancel_count={count} cancel_visible={} cancel_enabled={} cancel_in_screen={}",
            self.visible.token(),
            self.enabled.token(),
            self.in_screen
        )
    }
}

fn parse(xml: &str) -> Result<Summary, Refusal> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(Refusal::Capture);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let (mut roots, mut applications, mut count) = (0, 0, 0u8);
    let (mut passcode, mut proud, mut heading) = (false, false, false);
    let mut cancel = Button::default();
    loop {
        let event = reader.read_event().map_err(|_| Refusal::Capture)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() >= 128 {
                    return Err(Refusal::Capture);
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(Refusal::Capture);
                    }
                }
                let mut name = Token::Other;
                let mut label = Token::Other;
                let mut bundle = None;
                let mut button = Button::default();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| Refusal::Capture)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| Refusal::Capture)?;
                    match attr.key.as_ref() {
                        b"name" => name = Token::parse(&value),
                        b"label" => label = Token::parse(&value),
                        b"bundleId" => bundle = Some(value == SPRINGBOARD),
                        b"visible" => button.visible = Flag::parse(&value),
                        b"enabled" => button.enabled = Flag::parse(&value),
                        b"x" => button.rect[0] = value.parse().ok(),
                        b"y" => button.rect[1] = value.parse().ok(),
                        b"width" => button.rect[2] = value.parse().ok(),
                        b"height" => button.rect[3] = value.parse().ok(),
                        _ => {}
                    }
                }
                let kind = node.name();
                if matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeAlert" | b"XCUIElementTypeSheet"
                ) || name == Token::Trust
                    || label == Token::Trust
                {
                    return Err(Refusal::Screen);
                }
                let mut inside = stack.last().copied().unwrap_or(false);
                if kind.as_ref() == b"XCUIElementTypeApplication" {
                    applications += 1;
                    if applications != 1
                        || !(bundle == Some(true)
                            || (bundle.is_none()
                                && (name == Token::SpringBoard || label == Token::SpringBoard)))
                    {
                        return Err(Refusal::Screen);
                    }
                    inside = true;
                }
                if !inside
                    && (matches!(
                        kind.as_ref(),
                        b"XCUIElementTypeKey" | b"XCUIElementTypeSecureTextField"
                    ) || matches!(name, Token::PasscodeView | Token::ProudLock)
                        || label == Token::Heading)
                {
                    return Err(Refusal::Screen);
                }
                if inside {
                    passcode |=
                        kind.as_ref() == b"XCUIElementTypeOther" && name == Token::PasscodeView;
                    proud |= kind.as_ref() == b"XCUIElementTypeOther" && name == Token::ProudLock;
                    heading |=
                        kind.as_ref() == b"XCUIElementTypeStaticText" && label == Token::Heading;
                    if kind.as_ref() == b"XCUIElementTypeButton"
                        && (name == Token::Cancel || label == Token::Cancel)
                        && name != Token::Excluded
                        && label != Token::Excluded
                    {
                        count = (count + 1).min(2);
                        cancel = if count == 1 {
                            button
                        } else {
                            Button::default()
                        };
                    }
                }
                if start {
                    stack.push(inside);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(Refusal::Capture)?;
            }
            Event::Text(text) if text.as_ref().iter().any(|byte| !byte.is_ascii_whitespace()) => {
                return Err(Refusal::Capture);
            }
            Event::DocType(_) | Event::CData(_) | Event::GeneralRef(_) => {
                return Err(Refusal::Capture);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || !stack.is_empty() {
        return Err(Refusal::Capture);
    }
    if applications != 1 || !(passcode || (proud && heading)) {
        return Err(Refusal::Screen);
    }
    let view = pin_screen::parse(xml).map_err(|_| Refusal::Screen)?;
    if view.visible_lock != LockState::Locked || view.has_alert {
        return Err(Refusal::Screen);
    }
    if !view.layout_valid || view.keypad.is_none() {
        return Err(Refusal::Geometry);
    }
    let size = view.screen_size.ok_or(Refusal::Geometry)?;
    if !(200.0..=2000.0).contains(&size.0) || !(400.0..=2000.0).contains(&size.1) {
        return Err(Refusal::Geometry);
    }
    let progress = PasscodeProgress::parse(
        view.secure_field_value
            .as_deref()
            .ok_or(Refusal::Progress)?,
        6,
    )
    .map_err(|_| Refusal::Progress)?;
    if progress.entered != 0 {
        return Err(Refusal::Progress);
    }
    let in_screen = match cancel.rect {
        [Some(x), Some(y), Some(width), Some(height)] => {
            [x, y, width, height].iter().all(|n| n.is_finite())
                && x >= 0.0
                && y >= 0.0
                && width > 0.0
                && height > 0.0
                && x + width <= size.0
                && y + height <= size.1
        }
        _ => false,
    };
    Ok(Summary {
        count,
        visible: cancel.visible,
        enabled: cancel.enabled,
        in_screen,
    })
}

pub(super) fn empty_keypad(xml: &str) -> bool {
    parse(xml).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    const CANCEL: &str = r#"<XCUIElementTypeButton name="Cancel" visible="true" enabled="true" x="240" y="700" width="80" height="40"/>"#;
    const ONE: &str = "lock_ui_attrs=observed cancel_count=one cancel_visible=true cancel_enabled=true cancel_in_screen=true";
    const ZERO: &str = "lock_ui_attrs=observed cancel_count=zero cancel_visible=missing cancel_enabled=missing cancel_in_screen=false";

    fn tree(extra: &str) -> String {
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
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="true" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/>"#);
        xml.push_str(extra);
        xml.push_str("</XCUIElementTypeApplication></AppiumAUT>");
        xml
    }

    #[test]
    fn summarizes_exact_cancel_buttons_without_retaining_other_content() {
        assert_eq!(parse(&tree(CANCEL)).unwrap().line(), ONE);
        assert_eq!(parse(&tree("")).unwrap().line(), ZERO);
        assert_eq!(
            parse(&tree(&CANCEL.replace("name=", "label=")))
                .unwrap()
                .line(),
            ONE
        );
        assert_eq!(
            parse(&tree(&format!("{CANCEL}{CANCEL}"))).unwrap().line(),
            "lock_ui_attrs=observed cancel_count=multiple cancel_visible=missing cancel_enabled=missing cancel_in_screen=false"
        );
        for excluded in ["Emergency", "Delete", "Cancel later", "cancel"] {
            assert_eq!(
                parse(&tree(&CANCEL.replace("Cancel", excluded)))
                    .unwrap()
                    .line(),
                ZERO
            );
        }
        assert_eq!(
            parse(&tree(&CANCEL.replace(
                "name=\"Cancel\"",
                "name=\"Delete\" label=\"Cancel\""
            )))
            .unwrap()
            .line(),
            ZERO
        );
        let private = CANCEL.replace(
            "name=\"Cancel\"",
            "name=\"fixture-private-name\" label=\"Cancel\" value=\"fixture-private-value\"",
        );
        assert_eq!(parse(&tree(&private)).unwrap().line(), ONE);
        assert_eq!(
            parse(&tree(CANCEL).replace("</AppiumAUT>", &format!("{CANCEL}</AppiumAUT>")))
                .unwrap()
                .line(),
            ONE
        );
        assert_eq!(
            parse(&tree("").replace("</AppiumAUT>", &format!("{CANCEL}</AppiumAUT>")))
                .unwrap()
                .line(),
            ZERO
        );
        assert_eq!(
            parse(&tree(&CANCEL.replace(
                "XCUIElementTypeButton",
                "XCUIElementTypeStaticText"
            )))
            .unwrap()
            .line(),
            ZERO
        );
    }

    #[test]
    fn reports_only_fixed_flags_and_bounded_geometry() {
        for (attribute, replacement, expected) in [
            (
                "visible=\"true\"",
                "visible=\"false\"",
                "cancel_visible=false",
            ),
            (
                "enabled=\"true\"",
                "enabled=\"false\"",
                "cancel_enabled=false",
            ),
            ("visible=\"true\"", "", "cancel_visible=missing"),
            (
                "enabled=\"true\"",
                "enabled=\"fixture-private-value\"",
                "cancel_enabled=missing",
            ),
        ] {
            let line = parse(&tree(&CANCEL.replace(attribute, replacement)))
                .unwrap()
                .line();
            assert!(line.contains(expected));
            assert!(!line.contains("fixture-private"));
        }
        for invalid in [
            "x=\"-1\"",
            "x=\"NaN\"",
            "x=\"inf\"",
            "x=\"399\"",
            "",
            "x=\"invalid\"",
        ] {
            assert!(
                parse(&tree(&CANCEL.replace("x=\"240\"", invalid)))
                    .unwrap()
                    .line()
                    .ends_with("cancel_in_screen=false")
            );
        }
        for invalid in ["0", "-1", "900", "NaN"] {
            assert!(
                parse(&tree(
                    &CANCEL.replace("width=\"80\"", &format!("width=\"{invalid}\""))
                ))
                .unwrap()
                .line()
                .ends_with("cancel_in_screen=false")
            );
        }
        let hidden = tree(CANCEL).replace("visible=\"true\"", "visible=\"false\"");
        assert_eq!(
            parse(&hidden).unwrap().line(),
            "lock_ui_attrs=observed cancel_count=one cancel_visible=false cancel_enabled=true cancel_in_screen=true"
        );
    }

    #[test]
    fn refuses_unsafe_or_unproven_passcode_screens() {
        for (xml, expected) in [
            (
                tree(CANCEL).replace("0 of 6 digits", "1 of 6 digits"),
                Refusal::Progress,
            ),
            (
                tree(CANCEL).replace("0 of 6 digits", "0 of 4 digits"),
                Refusal::Progress,
            ),
            (
                tree(CANCEL).replace("0 of 6 digits", "unreadable"),
                Refusal::Progress,
            ),
            (
                tree(CANCEL).replace("passcode-view", "unknown-view"),
                Refusal::Screen,
            ),
            (
                tree(CANCEL).replace("SpringBoard", "OtherApp"),
                Refusal::Screen,
            ),
            (
                tree(CANCEL).replace(
                    "name=\"SpringBoard\"",
                    "name=\"SpringBoard\" bundleId=\"com.example.other\"",
                ),
                Refusal::Screen,
            ),
            (tree("<XCUIElementTypeAlert/>"), Refusal::Screen),
            (tree("<XCUIElementTypeSheet/>"), Refusal::Screen),
            (
                tree("<XCUIElementTypeStaticText label=\"Trust This Computer?\"/>"),
                Refusal::Screen,
            ),
            (
                tree(CANCEL).replace("name=\"1\"", "name=\"missing-key\""),
                Refusal::Geometry,
            ),
            (
                tree(CANCEL).replace("x=\"40\"", "x=\"-1\""),
                Refusal::Geometry,
            ),
            (tree("<XCUIElementTypeSecureTextField/>"), Refusal::Screen),
            (
                tree("<XCUIElementTypeApplication name=\"SpringBoard\"/>"),
                Refusal::Screen,
            ),
            (
                tree(CANCEL).replace("</AppiumAUT>", "<XCUIElementTypeKey/></AppiumAUT>"),
                Refusal::Screen,
            ),
        ] {
            assert_eq!(parse(&xml).err(), Some(expected));
        }
    }

    #[test]
    fn rejects_malformed_xml_without_echoing_content() {
        for xml in [
            String::new(),
            "<AppiumAUT>".into(),
            "<a></b>".into(),
            format!("{}<extra/>", tree(CANCEL)),
            tree(&CANCEL.replace("name=\"Cancel\"", "name=\"Cancel\" name=\"private\"")),
            tree(CANCEL).replace("</AppiumAUT>", "private text</AppiumAUT>"),
            tree("<![CDATA[private text]]>"),
            tree("&amp;"),
            format!("<!DOCTYPE private>{}", tree(CANCEL)),
            tree(&format!(
                "{}{}",
                "<node>".repeat(130),
                "</node>".repeat(130)
            )),
            " ".repeat(2 * 1024 * 1024 + 1),
        ] {
            assert_eq!(parse(&xml).err(), Some(Refusal::Capture));
        }
    }

    enum Step {
        Identity(Result<(), Refusal>),
        Native(NativeLock),
        Get(&'static str, Result<Value, Refusal>),
    }

    struct Fake {
        steps: VecDeque<Step>,
        calls: usize,
    }

    impl Fake {
        fn new() -> Self {
            let locked = NativeLock {
                state: LockState::Locked,
                unlocked_since_boot: Some(true),
            };
            let status = json!({"sessionId":"fixture-session", "value":{"ready":true}});
            let app = json!({"value":{"bundleId":SPRINGBOARD}});
            Self {
                steps: VecDeque::from([
                    Step::Identity(Ok(())),
                    Step::Native(locked),
                    Step::Get("/status", Ok(status.clone())),
                    Step::Get("/session/fixture-session", Ok(json!({"value":{}}))),
                    Step::Get("/wda/locked", Ok(json!({"value":true}))),
                    Step::Get("/wda/activeAppInfo", Ok(app.clone())),
                    Step::Get(SOURCE, Ok(json!({"value":tree(CANCEL)}))),
                    Step::Get("/wda/locked", Ok(json!({"value":true}))),
                    Step::Get("/wda/activeAppInfo", Ok(app)),
                    Step::Get("/status", Ok(status)),
                    Step::Get("/session/fixture-session", Ok(json!({"value":{}}))),
                    Step::Native(locked),
                    Step::Identity(Ok(())),
                ]),
                calls: 0,
            }
        }

        fn next(&mut self) -> Step {
            self.calls += 1;
            self.steps
                .pop_front()
                .expect("unexpected extra device access")
        }
    }

    impl ReadDevice for Fake {
        fn identity(&mut self) -> Result<(), Refusal> {
            let Step::Identity(result) = self.next() else {
                panic!("expected identity check")
            };
            result
        }
        fn native(&mut self) -> NativeLock {
            let Step::Native(result) = self.next() else {
                panic!("expected native check")
            };
            result
        }
        fn get(&mut self, path: &str) -> Result<Value, Refusal> {
            let Step::Get(expected, result) = self.next() else {
                panic!("expected GET")
            };
            assert_eq!(path, expected);
            result
        }
    }

    #[test]
    fn read_only_flow_checks_both_sides_of_one_xml_capture() {
        let mut fake = Fake::new();
        assert_eq!(inspect(&mut fake).unwrap().line(), ONE);
        assert!(fake.steps.is_empty());
        assert_eq!(fake.calls, 13);
        for index in [0, 12] {
            let mut fake = Fake::new();
            fake.steps[index] = Step::Identity(Err(Refusal::Identity));
            assert_eq!(inspect(&mut fake).err(), Some(Refusal::Identity));
            assert_eq!(fake.calls, index + 1);
        }
        for index in [1, 11] {
            for (state, boot, reason) in [
                (LockState::Unlocked, Some(true), Refusal::NativeLock),
                (LockState::Uncertain, Some(true), Refusal::NativeLock),
                (LockState::Locked, Some(false), Refusal::FirstUnlock),
                (LockState::Locked, None, Refusal::FirstUnlock),
            ] {
                let mut fake = Fake::new();
                fake.steps[index] = Step::Native(NativeLock {
                    state,
                    unlocked_since_boot: boot,
                });
                assert_eq!(inspect(&mut fake).err(), Some(reason));
                assert_eq!(fake.calls, index + 1);
            }
        }
        for (index, path, reason) in [
            (2, "/status", Refusal::Session),
            (3, "/session/fixture-session", Refusal::Session),
            (4, "/wda/locked", Refusal::WdaLock),
            (5, "/wda/activeAppInfo", Refusal::Screen),
            (6, SOURCE, Refusal::Capture),
            (7, "/wda/locked", Refusal::WdaLock),
            (8, "/wda/activeAppInfo", Refusal::Screen),
            (9, "/status", Refusal::Session),
            (10, "/session/fixture-session", Refusal::Session),
        ] {
            let mut fake = Fake::new();
            fake.steps[index] = Step::Get(path, Err(Refusal::Capture));
            assert_eq!(inspect(&mut fake).err(), Some(reason));
            assert_eq!(fake.calls, index + 1);
        }
    }

    #[test]
    fn refuses_unready_changed_sessions_and_unlocked_or_wrong_foreground() {
        for value in [json!({"ready":false}), json!({})] {
            let mut fake = Fake::new();
            fake.steps[2] = Step::Get(
                "/status",
                Ok(json!({"sessionId":"fixture-session","value":value})),
            );
            assert_eq!(inspect(&mut fake).err(), Some(Refusal::Session));
        }
        for id in ["", "../private", "bad id"] {
            let mut fake = Fake::new();
            fake.steps[2] = Step::Get(
                "/status",
                Ok(json!({"sessionId":id,"value":{"ready":true}})),
            );
            assert_eq!(inspect(&mut fake).err(), Some(Refusal::Session));
        }
        let mut fake = Fake::new();
        fake.steps[9] = Step::Get(
            "/status",
            Ok(json!({"sessionId":"replacement","value":{"ready":true}})),
        );
        fake.steps[10] = Step::Get("/session/replacement", Ok(json!({"value":{}})));
        assert_eq!(inspect(&mut fake).err(), Some(Refusal::Session));
        for index in [4, 7] {
            for value in [json!(false), Value::Null, json!("true")] {
                let mut fake = Fake::new();
                fake.steps[index] = Step::Get("/wda/locked", Ok(json!({"value":value})));
                assert_eq!(inspect(&mut fake).err(), Some(Refusal::WdaLock));
            }
        }
        for index in [5, 8] {
            let mut fake = Fake::new();
            fake.steps[index] = Step::Get(
                "/wda/activeAppInfo",
                Ok(json!({"value":{"bundleId":"com.example.other"}})),
            );
            assert_eq!(inspect(&mut fake).err(), Some(Refusal::Screen));
        }
    }

    #[test]
    fn errors_and_arguments_have_only_fixed_output() {
        for (reason, token) in [
            (Refusal::Identity, "identity_unproven"),
            (Refusal::NativeLock, "native_lock_unproven"),
            (Refusal::FirstUnlock, "first_unlock_unproven"),
            (Refusal::Session, "session_unproven"),
            (Refusal::WdaLock, "wda_lock_unproven"),
            (Refusal::Screen, "screen_unproven"),
            (Refusal::Geometry, "geometry_unproven"),
            (Refusal::Progress, "progress_unproven"),
            (Refusal::Capture, "capture_failed"),
        ] {
            assert_eq!(
                format!("lock_ui_attrs={}", reason.message()),
                format!("lock_ui_attrs=refused reason={token}")
            );
        }
        for (status, body) in [
            (500, b"private error".as_slice()),
            (200, b"invalid private json"),
            (200, br#"{"value":{"error":"private error"}}"#),
            (200, b"{} "),
        ] {
            assert_eq!(response_json(status, body).err(), Some(Refusal::Capture));
        }
        assert_eq!(command(&[]), Err("invalid_arguments"));
        assert_eq!(
            command(&["--other".into(), "private".into()]),
            Err("invalid_arguments")
        );
        assert_eq!(
            command(&["--config".into(), ".".into()]),
            Err("configuration_invalid")
        );
    }
}
