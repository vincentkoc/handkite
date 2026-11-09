use handkite_core::controller::{DeviceAdapter, RawObservation};
use handkite_core::protocol::{HardwareId, LockState};
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::{host_config, native_lock};
use quick_xml::events::Event;
use std::path::Path;

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("invalid_arguments");
    };
    if flag != "--config" {
        return Err("invalid_arguments");
    }
    let binding = host_config::load(Path::new(path)).map_err(|_| "configuration_invalid")?;
    let mut adapter = WdaAdapter::new(binding.usb_id.clone()).map_err(|_| "refused")?;
    adapter.identities().map_err(|_| "refused")?;
    if native_lock::query(&binding.usb_id).state != LockState::Unlocked {
        return Err("refused");
    }
    let (observation, _) = adapter.observe_with_attributes().map_err(|_| "refused")?;
    let native = native_lock::query(&binding.usb_id).state;
    adapter.identities().map_err(|_| "refused")?;
    let summary = inspect(&observation, &binding.usb_id, native).map_err(|_| "refused")?;
    println!("{}", summary.line());
    Ok(())
}

fn inspect(raw: &RawObservation, expected: &HardwareId, native: LockState) -> Result<Summary, ()> {
    if raw.connected_usb != *expected
        || raw.wda_target != *expected
        || native != LockState::Unlocked
        || raw.state.lock != LockState::Unlocked
        || !raw.state.wda_ready
        || raw.state.foreground_app.as_deref() != Some("com.apple.mobiletimer")
    {
        return Err(());
    }
    parse(&raw.accessibility_tree)
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

#[derive(Default)]
enum Count {
    #[default]
    Zero,
    One,
    Multiple,
}

#[derive(Default)]
struct Attributes {
    visible: Flag,
    enabled: Flag,
    selected: Flag,
}

#[derive(Default)]
struct Buttons {
    count: Count,
    attributes: Attributes,
}

impl Buttons {
    fn add(&mut self, attributes: Attributes) {
        match self.count {
            Count::Zero => {
                self.count = Count::One;
                self.attributes = attributes;
            }
            Count::One | Count::Multiple => {
                self.count = Count::Multiple;
                self.attributes = Attributes::default();
            }
        }
    }

    fn count_token(&self) -> &'static str {
        match self.count {
            Count::Zero => "zero",
            Count::One => "one",
            Count::Multiple => "multiple",
        }
    }
}

#[derive(Default)]
struct Summary {
    stopwatch: Buttons,
    world_clock: Buttons,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            "clock_attrs=observed stopwatch_count={} stopwatch_visible={} stopwatch_enabled={} stopwatch_selected={} world_clock_count={} world_clock_visible={} world_clock_enabled={} world_clock_selected={}",
            self.stopwatch.count_token(),
            self.stopwatch.attributes.visible.token(),
            self.stopwatch.attributes.enabled.token(),
            self.stopwatch.attributes.selected.token(),
            self.world_clock.count_token(),
            self.world_clock.attributes.visible.token(),
            self.world_clock.attributes.enabled.token(),
            self.world_clock.attributes.selected.token(),
        )
    }
}

fn parse(xml: &str) -> Result<Summary, ()> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(());
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut roots = 0;
    let mut summary = Summary::default();
    loop {
        let event = reader.read_event().map_err(|_| ())?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() > 128 {
                    return Err(());
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(());
                    }
                }
                if node.name().as_ref() == b"XCUIElementTypeButton" && stack.contains(&true) {
                    let mut target = None;
                    let mut attributes = Attributes::default();
                    for attr in node.attributes() {
                        let attr = attr.map_err(|_| ())?;
                        let value = attr
                            .decode_and_unescape_value(reader.decoder())
                            .map_err(|_| ())?;
                        match attr.key.as_ref() {
                            b"name" => {
                                target = match value.as_ref() {
                                    "Stopwatch" => Some(true),
                                    "World Clock" => Some(false),
                                    _ => None,
                                }
                            }
                            b"visible" => attributes.visible = Flag::parse(&value),
                            b"enabled" => attributes.enabled = Flag::parse(&value),
                            b"selected" => attributes.selected = Flag::parse(&value),
                            _ => {}
                        }
                    }
                    match target {
                        Some(true) => summary.stopwatch.add(attributes),
                        Some(false) => summary.world_clock.add(attributes),
                        None => {}
                    }
                }
                if is_start {
                    stack.push(node.name().as_ref() == b"XCUIElementTypeTabBar");
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(())?;
            }
            Event::Text(text) if text.as_ref().iter().any(|byte| !byte.is_ascii_whitespace()) => {
                return Err(());
            }
            Event::DocType(_) | Event::CData(_) => return Err(()),
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || !stack.is_empty() {
        return Err(());
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{DeviceState, ImageFormat, ObservationWindow, ScreenImage};

    fn tree(buttons: &str) -> String {
        format!(
            "<AppiumAUT><XCUIElementTypeApplication><XCUIElementTypeTabBar>{buttons}</XCUIElementTypeTabBar></XCUIElementTypeApplication></AppiumAUT>"
        )
    }

    #[test]
    fn exact_output_contains_only_categories_for_the_two_tab_buttons() {
        let xml = tree(
            r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="false" selected="invalid-sensitive-value" x="123" label="private fixture"/><XCUIElementTypeButton name="World Clock" visible="false" enabled="true" selected="true"/>"#,
        );
        assert_eq!(
            parse(&xml).unwrap().line(),
            "clock_attrs=observed stopwatch_count=one stopwatch_visible=true stopwatch_enabled=false stopwatch_selected=missing world_clock_count=one world_clock_visible=false world_clock_enabled=true world_clock_selected=true"
        );
    }

    #[test]
    fn absent_duplicate_and_outside_tabbar_buttons_cannot_supply_unique_attributes() {
        let outside = r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="true" selected="true"/>"#;
        let absent = format!(
            "<AppiumAUT>{outside}<XCUIElementTypeTabBar><XCUIElementTypeStaticText name=\"Stopwatch\"/></XCUIElementTypeTabBar></AppiumAUT>"
        );
        assert_eq!(
            parse(&absent).unwrap().line(),
            "clock_attrs=observed stopwatch_count=zero stopwatch_visible=missing stopwatch_enabled=missing stopwatch_selected=missing world_clock_count=zero world_clock_visible=missing world_clock_enabled=missing world_clock_selected=missing"
        );
        let duplicate = tree(&format!(
            "{outside}{outside}<XCUIElementTypeButton name=\"World Clock\"/>"
        ));
        assert_eq!(
            parse(&duplicate).unwrap().line(),
            "clock_attrs=observed stopwatch_count=multiple stopwatch_visible=missing stopwatch_enabled=missing stopwatch_selected=missing world_clock_count=one world_clock_visible=missing world_clock_enabled=missing world_clock_selected=missing"
        );
        assert_eq!(
            parse(&tree(
                r#"<XCUIElementTypeButton name="Stopwatch" selected="false"/>"#
            ))
            .unwrap()
            .stopwatch
            .attributes
            .selected
            .token(),
            "false"
        );
    }

    #[test]
    fn malformed_or_unbounded_trees_are_refused() {
        for xml in [
            "".to_owned(),
            "<root>".into(),
            "<a></b>".into(),
            "<a/><b/>".into(),
            "<!DOCTYPE a><a/>".into(),
            tree(r#"<XCUIElementTypeButton name="Stopwatch" visible="true" visible="false"/>"#),
            " ".repeat(2 * 1024 * 1024 + 1),
        ] {
            assert!(parse(&xml).is_err());
        }
    }

    fn observation() -> RawObservation {
        RawObservation {
            connected_usb: HardwareId::new("fixture-device").unwrap(),
            wda_target: HardwareId::new("fixture-device").unwrap(),
            window: ObservationWindow {
                started_unix_ms: 1,
                finished_unix_ms: 2,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some("com.apple.mobiletimer".into()),
            },
            accessibility_tree: tree(""),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: Vec::new(),
            },
        }
    }

    #[test]
    fn output_requires_pinned_identity_native_and_wda_unlock_and_clock() {
        let expected = HardwareId::new("fixture-device").unwrap();
        assert!(inspect(&observation(), &expected, LockState::Unlocked).is_ok());
        for native in [LockState::Locked, LockState::Uncertain] {
            assert!(inspect(&observation(), &expected, native).is_err());
        }
        let mutations: &[fn(&mut RawObservation)] = &[
            |raw| raw.connected_usb = HardwareId::new("other-device").unwrap(),
            |raw| raw.wda_target = HardwareId::new("other-device").unwrap(),
            |raw| raw.state.lock = LockState::Locked,
            |raw| raw.state.lock = LockState::Uncertain,
            |raw| raw.state.wda_ready = false,
            |raw| raw.state.foreground_app = Some("com.apple.springboard".into()),
            |raw| raw.state.foreground_app = None,
        ];
        for mutate in mutations {
            let mut raw = observation();
            mutate(&mut raw);
            assert!(inspect(&raw, &expected, LockState::Unlocked).is_err());
        }
    }
}
