use super::lock_ui_attrs::{LockedXml, capture_with_hittable, empty_keypad};
use handkite_core::protocol::HardwareId;
use handkite_host::{host_config, pin_screen};
use quick_xml::events::Event;
use std::path::Path;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(5);
const MARGIN: f64 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Flag {
    True,
    False,
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
#[derive(Clone, Copy, Debug, PartialEq)]
enum Refusal {
    Capture,
    Freshness,
    Session,
    Screen,
    Missing,
    Multiple,
    Attributes([Flag; 3]),
    Geometry,
    Separation,
    Changed,
}
impl Refusal {
    fn message(self) -> String {
        match self {
            Self::Capture => "refused reason=capture_unproven".into(),
            Self::Freshness => "refused reason=freshness_unproven".into(),
            Self::Session => "refused reason=session_unproven".into(),
            Self::Screen => "refused reason=screen_unproven".into(),
            Self::Missing => "refused reason=target_unproven count=zero".into(),
            Self::Multiple => "refused reason=target_unproven count=multiple".into(),
            Self::Attributes([v, e, h]) => format!(
                "refused reason=attributes_unproven count=one visible={} enabled={} hittable={}",
                v.token(),
                e.token(),
                h.token()
            ),
            Self::Geometry => "refused reason=geometry_unproven".into(),
            Self::Separation => "refused reason=separation_unproven".into(),
            Self::Changed => "refused reason=changed_geometry".into(),
        }
    }
}

pub(super) fn command(args: &[String]) -> Result<(), String> {
    let [flag, path] = args else {
        return Err("refused reason=invalid_arguments".into());
    };
    if flag != "--config" {
        return Err("refused reason=invalid_arguments".into());
    }
    let expected = host_config::load(Path::new(path))
        .map_err(|_| "refused reason=configuration_invalid")?
        .usb_id;
    let summary = inspect(&mut Live(expected)).map_err(Refusal::message)?;
    println!("{}", summary.line());
    Ok(())
}

// Only XML capture is exposed; no settings, input or marker operation exists here.
trait ReadDevice {
    fn capture(&mut self) -> Result<LockedXml, Refusal>;
    fn now(&self) -> Instant;
}
struct Live(HardwareId);
impl ReadDevice for Live {
    fn capture(&mut self) -> Result<LockedXml, Refusal> {
        capture_with_hittable(self.0.clone()).map_err(|_| Refusal::Capture)
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
}
fn fresh(capture: &LockedXml, now: Instant) -> Result<(), Refusal> {
    if capture.started > capture.finished
        || capture.finished > now
        || now.duration_since(capture.started) >= BUDGET
    {
        return Err(Refusal::Freshness);
    }
    if capture.session.is_empty()
        || capture.session.len() > 128
        || !capture
            .session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Refusal::Session);
    }
    Ok(())
}
fn inspect(device: &mut impl ReadDevice) -> Result<Summary, Refusal> {
    let first = device.capture()?;
    fresh(&first, device.now())?;
    let before = parse(&first.xml)?;
    fresh(&first, device.now())?;
    let second = device.capture()?;
    fresh(&second, device.now())?;
    if first.session != second.session {
        return Err(Refusal::Session);
    }
    if second.started < first.finished
        || second.started <= first.started
        || second.finished <= first.finished
    {
        return Err(Refusal::Freshness);
    }
    let after = parse(&second.xml)?;
    fresh(&second, device.now())?;
    if before != after {
        return Err(Refusal::Changed);
    }
    Ok(after)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect([f64; 4]);
impl Rect {
    fn read(raw: [Option<f64>; 4], screen: (f64, f64)) -> Result<Self, Refusal> {
        let [Some(x), Some(y), Some(w), Some(h)] = raw else {
            return Err(Refusal::Geometry);
        };
        if ![x, y, w, h].iter().all(|v| v.is_finite())
            || x < 0.0
            || y < 0.0
            || w <= 0.0
            || h <= 0.0
            || x + w > screen.0
            || y + h > screen.1
        {
            return Err(Refusal::Geometry);
        }
        Ok(Self([x, y, w, h]))
    }
    fn separate(self, other: Self) -> bool {
        let [x, y, w, h] = self.0;
        let [ox, oy, ow, oh] = other.0;
        x + w + MARGIN <= ox
            || ox + ow + MARGIN <= x
            || y + h + MARGIN <= oy
            || oy + oh + MARGIN <= y
    }
}
#[derive(Debug, PartialEq)]
struct Summary {
    screen: (f64, f64),
    target: Rect,
    other_kind: bool,
    obstacles: Vec<Rect>,
}
impl Summary {
    fn line(&self) -> String {
        let [x, y, w, h] = self.target.0;
        let norm = |value: f64, total: f64| (value / total * 1000.0).round() as u16;
        format!(
            "assistive_attrs=observed count=one kind={} visible=true enabled=true hittable=true bounded=true separated=true rect_permille={},{},{},{}",
            if self.other_kind { "other" } else { "button" },
            norm(x, self.screen.0),
            norm(y, self.screen.1),
            norm(w, self.screen.0),
            norm(h, self.screen.1)
        )
    }
}
fn parse(xml: &str) -> Result<Summary, Refusal> {
    // Reuse strict bounded XML, exact SpringBoard identity, no-alert, unique field/key,
    // valid keypad and empty-six-digit progress checks before considering a target.
    if !empty_keypad(xml) {
        return Err(Refusal::Screen);
    }
    let screen = pin_screen::parse(xml)
        .map_err(|_| Refusal::Screen)?
        .screen_size
        .ok_or(Refusal::Geometry)?;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut target = None;
    let mut obstacles = Vec::new();
    let (mut keys_and_field, mut cancels, mut emergencies) = (0, 0, 0);
    loop {
        let event = reader.read_event().map_err(|_| Refusal::Screen)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                let (mut name, mut label) = (None, None);
                let mut flags = [Flag::Missing; 3];
                let mut rect = [None; 4];
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| Refusal::Screen)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| Refusal::Screen)?;
                    match attr.key.as_ref() {
                        b"name" => name = Some(value.into_owned()),
                        b"label" => label = Some(value.into_owned()),
                        b"visible" => flags[0] = Flag::parse(&value),
                        b"enabled" => flags[1] = Flag::parse(&value),
                        b"hittable" => flags[2] = Flag::parse(&value),
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
                let named =
                    |value: &str| name.as_deref() == Some(value) || label.as_deref() == Some(value);
                if kind.as_ref() == b"XCUIElementTypeTextField" {
                    return Err(Refusal::Screen);
                }
                if kind.as_ref() == b"XCUIElementTypeApplication"
                    && rect != [Some(0.0), Some(0.0), Some(screen.0), Some(screen.1)]
                {
                    return Err(Refusal::Geometry);
                }
                if named("AssistiveTouch") {
                    if target.is_some() {
                        return Err(Refusal::Multiple);
                    }
                    if !inside
                        || !matches!(
                            kind.as_ref(),
                            b"XCUIElementTypeButton" | b"XCUIElementTypeOther"
                        )
                        || named("Cancel")
                        || named("Emergency")
                        || named("Delete")
                    {
                        return Err(Refusal::Screen);
                    }
                    target = Some((
                        Rect::read(rect, screen)?,
                        flags,
                        kind.as_ref() == b"XCUIElementTypeOther",
                    ));
                } else if matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeKey" | b"XCUIElementTypeSecureTextField"
                ) || super::cancel_trial::competing_control(kind.as_ref())
                {
                    if !inside {
                        return Err(Refusal::Screen);
                    }
                    keys_and_field += usize::from(matches!(
                        kind.as_ref(),
                        b"XCUIElementTypeKey" | b"XCUIElementTypeSecureTextField"
                    ));
                    cancels +=
                        usize::from(named("Cancel") && kind.as_ref() == b"XCUIElementTypeButton");
                    emergencies += usize::from(
                        named("Emergency") && kind.as_ref() == b"XCUIElementTypeButton",
                    );
                    obstacles.push(Rect::read(rect, screen)?);
                }
                if start {
                    stack.push(inside);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(Refusal::Screen)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let (target, flags, other_kind) = target.ok_or(Refusal::Missing)?;
    if flags != [Flag::True; 3] {
        return Err(Refusal::Attributes(flags));
    }
    if keys_and_field != 11 || cancels != 1 || emergencies != 1 {
        return Err(Refusal::Geometry);
    }
    if obstacles.iter().any(|rect| !target.separate(*rect)) {
        return Err(Refusal::Separation);
    }
    Ok(Summary {
        screen,
        target,
        other_kind,
        obstacles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const TARGET: &str = r#"<XCUIElementTypeButton name="AssistiveTouch" visible="true" enabled="true" hittable="true" x="330" y="80" width="40" height="40"/>"#;
    const CANCEL: &str = r#"<XCUIElementTypeButton name="Cancel" visible="false" enabled="true" x="280" y="700" width="100" height="40"/>"#;
    const EMERGENCY: &str = r#"<XCUIElementTypeButton name="Emergency" visible="false" enabled="true" x="20" y="700" width="100" height="40"/>"#;
    const LINE: &str = "assistive_attrs=observed count=one kind=button visible=true enabled=true hittable=true bounded=true separated=true rect_permille=825,100,100,50";

    fn tree(target: &str) -> String {
        let mut xml = String::from(
            r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" x="0" y="0" width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..=9 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="false" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + column * 100, 300 + row * 100));
        }
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="false" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/>"#);
        xml.push_str(CANCEL);
        xml.push_str(EMERGENCY);
        xml.push_str(target);
        xml.push_str("</XCUIElementTypeApplication></AppiumAUT>");
        xml
    }

    #[test]
    fn exact_target_has_only_fixed_categories_and_normalized_geometry() {
        for target in [TARGET.to_owned(), TARGET.replace("name=", "label=")] {
            assert_eq!(parse(&tree(&target)).unwrap().line(), LINE);
        }
        assert_eq!(
            parse(&tree(
                &TARGET.replace("XCUIElementTypeButton", "XCUIElementTypeOther")
            ))
            .unwrap()
            .line(),
            LINE.replace("kind=button", "kind=other")
        );
        let private = TARGET.replace("name=\"AssistiveTouch\"", "name=\"fixture-private-name\" label=\"AssistiveTouch\" value=\"fixture-private-value\"");
        assert_eq!(parse(&tree(&private)).unwrap().line(), LINE);
        assert_eq!(parse(&tree("")).unwrap_err(), Refusal::Missing);
        assert_eq!(
            parse(&tree(&format!("{TARGET}{TARGET}"))).unwrap_err(),
            Refusal::Multiple
        );
        for wrong in ["Assistive Touch", "assistivetouch", "AssistiveTouch menu"] {
            assert_eq!(
                parse(&tree(&TARGET.replace("AssistiveTouch", wrong))).unwrap_err(),
                Refusal::Missing
            );
        }
    }

    #[test]
    fn all_flags_must_be_explicit_true_and_errors_never_echo_values() {
        for (index, attribute) in ["visible", "enabled", "hittable"].into_iter().enumerate() {
            for (replacement, expected) in [
                (format!("{attribute}=\"false\""), Flag::False),
                (String::new(), Flag::Missing),
                (
                    format!("{attribute}=\"fixture-private-value\""),
                    Flag::Missing,
                ),
            ] {
                let mut flags = [Flag::True; 3];
                flags[index] = expected;
                let reason = parse(&tree(
                    &TARGET.replace(&format!("{attribute}=\"true\""), &replacement),
                ))
                .unwrap_err();
                assert_eq!(reason, Refusal::Attributes(flags));
                assert!(!reason.message().contains("fixture-private"));
            }
        }
        assert_eq!(
            Refusal::Attributes([Flag::True, Flag::False, Flag::Missing]).message(),
            "refused reason=attributes_unproven count=one visible=true enabled=false hittable=missing"
        );
        for (reason, output) in [
            (Refusal::Capture, "capture_unproven"),
            (Refusal::Freshness, "freshness_unproven"),
            (Refusal::Session, "session_unproven"),
            (Refusal::Screen, "screen_unproven"),
            (Refusal::Geometry, "geometry_unproven"),
            (Refusal::Separation, "separation_unproven"),
            (Refusal::Changed, "changed_geometry"),
        ] {
            assert_eq!(reason.message(), format!("refused reason={output}"));
        }
        assert_eq!(
            Refusal::Missing.message(),
            "refused reason=target_unproven count=zero"
        );
        assert_eq!(
            Refusal::Multiple.message(),
            "refused reason=target_unproven count=multiple"
        );
        assert_eq!(
            command(&[]).unwrap_err(),
            "refused reason=invalid_arguments"
        );
    }

    #[test]
    fn requires_bounded_controls_and_eight_point_margin() {
        for invalid in ["", "x=\"-1\"", "x=\"NaN\"", "x=\"inf\"", "x=\"390\""] {
            assert_eq!(
                parse(&tree(&TARGET.replace("x=\"330\"", invalid))).unwrap_err(),
                Refusal::Geometry
            );
        }
        assert!(parse(&tree(&TARGET.replace("y=\"80\"", "y=\"172\""))).is_ok());
        assert_eq!(
            parse(&tree(&TARGET.replace("y=\"80\"", "y=\"173\""))).unwrap_err(),
            Refusal::Separation
        );
        for control in [CANCEL, EMERGENCY] {
            let overlapping = control
                .replace("y=\"700\"", "y=\"80\"")
                .replace("x=\"20\"", "x=\"320\"")
                .replace("x=\"280\"", "x=\"300\"")
                .replace("width=\"100\"", "width=\"60\"");
            assert_eq!(
                parse(&tree(TARGET).replace(control, &overlapping)).unwrap_err(),
                Refusal::Separation
            );
            assert_eq!(
                parse(&tree(TARGET).replace(control, &control.replace("height=\"40\"", "")))
                    .unwrap_err(),
                Refusal::Geometry
            );
            assert_eq!(
                parse(&tree(TARGET).replace(control, "")).unwrap_err(),
                Refusal::Geometry
            );
        }
        for extra in [
            r#"<XCUIElementTypeButton name="fixture-private-name" visible="false" enabled="true"/>"#,
            r#"<XCUIElementTypeSlider/>"#,
        ] {
            assert_eq!(
                parse(&tree(&format!("{TARGET}{extra}"))).unwrap_err(),
                Refusal::Geometry
            );
        }
        let overlap = TARGET.replace("AssistiveTouch", "fixture-private-name");
        assert_eq!(
            parse(&tree(&format!("{TARGET}{overlap}"))).unwrap_err(),
            Refusal::Separation
        );
    }

    #[test]
    fn rejects_unsafe_screens_and_malformed_or_duplicate_xml() {
        for xml in [
            tree(TARGET).replace("0 of 6 digits", "1 of 6 digits"),
            tree(TARGET).replace("0 of 6 digits", "0 of 4 digits"),
            tree(TARGET).replace("SpringBoard", "fixture-private-app"),
            tree(&format!("{TARGET}<XCUIElementTypeAlert/>")),
            tree(&format!("{TARGET}<XCUIElementTypeTextField/>")),
            tree(&format!("{TARGET}<XCUIElementTypeButton name=\"Trust\"/>")),
            tree(&TARGET.replace("hittable=\"true\"", "hittable=\"true\" hittable=\"false\"")),
            tree(&TARGET.replace("XCUIElementTypeButton", "XCUIElementTypeStaticText")),
            tree("").replace("</AppiumAUT>", &format!("{TARGET}</AppiumAUT>")),
            tree(&format!("{TARGET}<![CDATA[fixture-private-value]]>")),
            format!("{}<extra/>", tree(TARGET)),
            "<a></b>".into(),
            " ".repeat(2 * 1024 * 1024 + 1),
        ] {
            assert_eq!(parse(&xml).unwrap_err(), Refusal::Screen);
        }
    }

    struct Fake {
        captures: VecDeque<Result<LockedXml, Refusal>>,
        now: Instant,
        calls: usize,
    }
    impl Fake {
        fn new() -> Self {
            let now = Instant::now();
            let capture = |offset| LockedXml {
                xml: tree(TARGET),
                session: "fixture-session".into(),
                started: now + Duration::from_millis(offset),
                finished: now + Duration::from_millis(offset + 100),
            };
            Self {
                captures: VecDeque::from([Ok(capture(0)), Ok(capture(100))]),
                now,
                calls: 0,
            }
        }
    }
    impl ReadDevice for Fake {
        fn capture(&mut self) -> Result<LockedXml, Refusal> {
            self.calls += 1;
            let capture = self
                .captures
                .pop_front()
                .expect("unexpected repeated capture")?;
            self.now = capture.finished;
            Ok(capture)
        }
        fn now(&self) -> Instant {
            self.now
        }
    }

    #[test]
    fn two_distinct_fresh_captures_must_agree_without_retry() {
        let mut fake = Fake::new();
        assert_eq!(inspect(&mut fake).unwrap().line(), LINE);
        assert_eq!(fake.calls, 2);
        for (case, expected) in [
            (0, Refusal::Session),
            (1, Refusal::Freshness),
            (2, Refusal::Freshness),
            (3, Refusal::Changed),
            (4, Refusal::Freshness),
        ] {
            let mut fake = Fake::new();
            let first_started = fake.captures[0].as_ref().unwrap().started;
            let second = fake.captures[1].as_mut().unwrap();
            match case {
                0 => second.session = "other-session".into(),
                1 => second.started = first_started,
                2 => second.finished = second.started + BUDGET,
                3 => second.xml = tree(&TARGET.replace("x=\"330\"", "x=\"329\"")),
                4 => second.started -= Duration::from_millis(1),
                _ => unreachable!(),
            }
            assert_eq!(inspect(&mut fake).unwrap_err(), expected);
            assert_eq!(fake.calls, 2);
        }
        for index in [0, 1] {
            let mut fake = Fake::new();
            fake.captures[index] = Err(Refusal::Capture);
            assert_eq!(inspect(&mut fake).unwrap_err(), Refusal::Capture);
            assert_eq!(fake.calls, index + 1);
        }
        let mut fake = Fake::new();
        fake.captures[0].as_mut().unwrap().xml = tree("");
        assert_eq!(inspect(&mut fake).unwrap_err(), Refusal::Missing);
        assert_eq!(fake.calls, 1);
    }
}
