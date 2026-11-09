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

#[derive(Clone, Copy, Default, PartialEq)]
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
enum ValueClass {
    #[default]
    Missing,
    Zero,
    One,
    True,
    False,
    Other,
}

impl ValueClass {
    fn parse(value: &str) -> Self {
        match value {
            "0" => Self::Zero,
            "1" => Self::One,
            "true" => Self::True,
            "false" => Self::False,
            _ => Self::Other,
        }
    }

    fn token(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Zero => "0",
            Self::One => "1",
            Self::True => "true",
            Self::False => "false",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum TextToken {
    #[default]
    Missing,
    WorldClock,
    Stopwatch,
    Start,
    SelectedWorldClock,
    SelectedStopwatch,
    Other,
}

impl TextToken {
    fn parse(value: &str) -> Self {
        match value {
            "World Clock" => Self::WorldClock,
            "Stopwatch" => Self::Stopwatch,
            "Start" => Self::Start,
            "Selected, World Clock" => Self::SelectedWorldClock,
            "Selected, Stopwatch" => Self::SelectedStopwatch,
            _ => Self::Other,
        }
    }

    // Exact literal evidence only; false does not establish an unselected tab.
    fn selected_prefix(self, tab: Self) -> bool {
        matches!(
            (self, tab),
            (Self::SelectedWorldClock, Self::WorldClock)
                | (Self::SelectedStopwatch, Self::Stopwatch)
        )
    }
}

#[derive(Default)]
enum Count {
    #[default]
    Zero,
    One,
    Multiple,
}

impl Count {
    fn add(&mut self) {
        *self = match self {
            Self::Zero => Self::One,
            Self::One | Self::Multiple => Self::Multiple,
        };
    }

    fn token(&self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::One => "one",
            Self::Multiple => "multiple",
        }
    }
}

#[derive(Default)]
struct Attributes {
    visible: Flag,
    enabled: Flag,
    selected: Flag,
    value: ValueClass,
    label: TextToken,
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
        self.count.token()
    }
}

#[derive(Default)]
struct Summary {
    stopwatch: Buttons,
    world_clock: Buttons,
    world_clock_navigation: Count,
    world_clock_text: Count,
    stopwatch_navigation: Count,
    stopwatch_text: Count,
    start_button: Count,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            concat!(
                "clock_attrs=observed stopwatch_count={} stopwatch_visible={} stopwatch_enabled={} stopwatch_selected={} ",
                "world_clock_count={} world_clock_visible={} world_clock_enabled={} world_clock_selected={} ",
                "stopwatch_value_class={} stopwatch_label_selected_prefix={} world_clock_value_class={} world_clock_label_selected_prefix={} ",
                "world_clock_navigation_count={} world_clock_text_count={} stopwatch_navigation_count={} stopwatch_text_count={} start_button_count={}",
            ),
            self.stopwatch.count_token(),
            self.stopwatch.attributes.visible.token(),
            self.stopwatch.attributes.enabled.token(),
            self.stopwatch.attributes.selected.token(),
            self.world_clock.count_token(),
            self.world_clock.attributes.visible.token(),
            self.world_clock.attributes.enabled.token(),
            self.world_clock.attributes.selected.token(),
            self.stopwatch.attributes.value.token(),
            self.stopwatch
                .attributes
                .label
                .selected_prefix(TextToken::Stopwatch),
            self.world_clock.attributes.value.token(),
            self.world_clock
                .attributes
                .label
                .selected_prefix(TextToken::WorldClock),
            self.world_clock_navigation.token(),
            self.world_clock_text.token(),
            self.stopwatch_navigation.token(),
            self.stopwatch_text.token(),
            self.start_button.token(),
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
                let mut name = TextToken::Missing;
                let mut attributes = Attributes::default();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| ())?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| ())?;
                    match attr.key.as_ref() {
                        b"name" => {
                            name = TextToken::parse(&value);
                        }
                        b"visible" => attributes.visible = Flag::parse(&value),
                        b"enabled" => attributes.enabled = Flag::parse(&value),
                        b"selected" => attributes.selected = Flag::parse(&value),
                        b"value" => attributes.value = ValueClass::parse(&value),
                        b"label" => attributes.label = TextToken::parse(&value),
                        _ => {}
                    }
                }
                let inside_tabbar = stack.contains(&true);
                if node.name().as_ref() == b"XCUIElementTypeButton" && inside_tabbar {
                    match name {
                        TextToken::Stopwatch => summary.stopwatch.add(attributes),
                        TextToken::WorldClock => summary.world_clock.add(attributes),
                        _ => {}
                    }
                } else if !inside_tabbar && attributes.visible == Flag::True {
                    let matches = |token| name == token || attributes.label == token;
                    match node.name().as_ref() {
                        b"XCUIElementTypeNavigationBar" => {
                            if matches(TextToken::WorldClock) {
                                summary.world_clock_navigation.add();
                            }
                            if matches(TextToken::Stopwatch) {
                                summary.stopwatch_navigation.add();
                            }
                        }
                        b"XCUIElementTypeStaticText" => {
                            if matches(TextToken::WorldClock) {
                                summary.world_clock_text.add();
                            }
                            if matches(TextToken::Stopwatch) {
                                summary.stopwatch_text.add();
                            }
                        }
                        b"XCUIElementTypeButton"
                            if attributes.enabled == Flag::True && matches(TextToken::Start) =>
                        {
                            summary.start_button.add();
                        }
                        _ => {}
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

    const EMPTY_EXTRA: &str = " stopwatch_value_class=missing stopwatch_label_selected_prefix=false world_clock_value_class=missing world_clock_label_selected_prefix=false world_clock_navigation_count=zero world_clock_text_count=zero stopwatch_navigation_count=zero stopwatch_text_count=zero start_button_count=zero";

    fn screen(content: &str) -> String {
        format!(
            "<AppiumAUT><XCUIElementTypeApplication>{content}</XCUIElementTypeApplication></AppiumAUT>"
        )
    }

    fn tree(buttons: &str) -> String {
        screen(&format!(
            "<XCUIElementTypeTabBar>{buttons}</XCUIElementTypeTabBar>"
        ))
    }

    #[test]
    fn exact_output_contains_only_categories_for_the_two_tab_buttons() {
        let xml = tree(
            r#"<XCUIElementTypeButton name="Stopwatch" visible="true" enabled="false" selected="invalid-sensitive-value" x="123" label="private fixture"/><XCUIElementTypeButton name="World Clock" visible="false" enabled="true" selected="true"/>"#,
        );
        assert_eq!(
            parse(&xml).unwrap().line(),
            format!(
                "clock_attrs=observed stopwatch_count=one stopwatch_visible=true stopwatch_enabled=false stopwatch_selected=missing world_clock_count=one world_clock_visible=false world_clock_enabled=true world_clock_selected=true{EMPTY_EXTRA}"
            )
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
            format!(
                "clock_attrs=observed stopwatch_count=zero stopwatch_visible=missing stopwatch_enabled=missing stopwatch_selected=missing world_clock_count=zero world_clock_visible=missing world_clock_enabled=missing world_clock_selected=missing{EMPTY_EXTRA}"
            )
        );
        let duplicate = tree(&format!(
            "{outside}{outside}<XCUIElementTypeButton name=\"World Clock\"/>"
        ));
        assert_eq!(
            parse(&duplicate).unwrap().line(),
            format!(
                "clock_attrs=observed stopwatch_count=multiple stopwatch_visible=missing stopwatch_enabled=missing stopwatch_selected=missing world_clock_count=one world_clock_visible=missing world_clock_enabled=missing world_clock_selected=missing{EMPTY_EXTRA}"
            )
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
    fn positive_content_counts_are_bounded_and_exclude_tabbar_descendants() {
        let positive = r#"
            <XCUIElementTypeNavigationBar name="World Clock" visible="true"/>
            <XCUIElementTypeStaticText label="World Clock" visible="true"/>
            <XCUIElementTypeNavigationBar label="Stopwatch" visible="true"/>
            <XCUIElementTypeStaticText name="Stopwatch" visible="true"/>
            <XCUIElementTypeButton label="Start" visible="true" enabled="true"/>
        "#;
        let ignored = r#"
            <XCUIElementTypeNavigationBar name="World Clock extra" visible="true"/>
            <XCUIElementTypeStaticText name="World Clock" visible="false"/>
            <XCUIElementTypeNavigationBar name="Stopwatch" visible="true "/>
            <XCUIElementTypeStaticText name="Stopwatch"/>
            <XCUIElementTypeButton name="Start" visible="true" enabled="false"/>
            <XCUIElementTypeButton name="Start" visible="false" enabled="true"/>
            <XCUIElementTypeButton name="Start" visible="true"/>
            <XCUIElementTypeStaticText name="Start" visible="true" enabled="true"/>
        "#;
        let xml = screen(&format!(
            "{positive}{ignored}<XCUIElementTypeTabBar><node>{positive}</node></XCUIElementTypeTabBar>"
        ));
        let summary = parse(&xml).unwrap();
        assert_eq!(summary.world_clock_navigation.token(), "one");
        assert_eq!(summary.world_clock_text.token(), "one");
        assert_eq!(summary.stopwatch_navigation.token(), "one");
        assert_eq!(summary.stopwatch_text.token(), "one");
        assert_eq!(summary.start_button.token(), "one");
        assert!(summary.line().ends_with("world_clock_navigation_count=one world_clock_text_count=one stopwatch_navigation_count=one stopwatch_text_count=one start_button_count=one"));
        let repeated = parse(&screen(&positive.repeat(3))).unwrap();
        assert!(repeated.line().ends_with("world_clock_navigation_count=multiple world_clock_text_count=multiple stopwatch_navigation_count=multiple stopwatch_text_count=multiple start_button_count=multiple"));
    }

    #[test]
    fn tab_values_use_only_exact_literal_classes() {
        for (value, class) in [
            (None, "missing"),
            (Some("0"), "0"),
            (Some("1"), "1"),
            (Some("true"), "true"),
            (Some("false"), "false"),
            (Some("True"), "other"),
            (Some("1.0"), "other"),
            (Some(" 1"), "other"),
            (Some("private-fixture-value"), "other"),
        ] {
            let attribute = value
                .map(|value| format!("value=\"{value}\""))
                .unwrap_or_default();
            let xml = tree(&format!(
                "<XCUIElementTypeButton name=\"Stopwatch\" {attribute}/><XCUIElementTypeButton name=\"World Clock\" {attribute}/>"
            ));
            let summary = parse(&xml).unwrap();
            assert_eq!(summary.stopwatch.attributes.value.token(), class);
            assert_eq!(summary.world_clock.attributes.value.token(), class);
            assert!(!summary.line().contains("private-fixture-value"));
        }
    }

    #[test]
    fn selected_label_prefix_requires_the_exact_allowlisted_tab_label() {
        for (name, label, selected) in [
            ("Stopwatch", "Selected, Stopwatch", true),
            ("World Clock", "Selected, World Clock", true),
            ("Stopwatch", "Selected, World Clock", false),
            ("World Clock", "Selected, Stopwatch", false),
            ("Stopwatch", "Stopwatch", false),
            ("Stopwatch", "selected, Stopwatch", false),
            ("Stopwatch", "Selected, Stopwatch extra", false),
            ("World Clock", "Selected, private-fixture-label", false),
        ] {
            let button =
                format!("<XCUIElementTypeButton name=\"{name}\" label=\"{label}\" value=\"1\"/>");
            let summary = parse(&tree(&button)).unwrap();
            let attributes = if name == "Stopwatch" {
                &summary.stopwatch.attributes
            } else {
                &summary.world_clock.attributes
            };
            assert_eq!(
                attributes.label.selected_prefix(TextToken::parse(name)),
                selected
            );
            assert!(!summary.line().contains("private-fixture-label"));
            let duplicate = parse(&tree(&button.repeat(2))).unwrap();
            let attributes = if name == "Stopwatch" {
                &duplicate.stopwatch.attributes
            } else {
                &duplicate.world_clock.attributes
            };
            assert!(!attributes.label.selected_prefix(TextToken::parse(name)));
            assert_eq!(attributes.value.token(), "missing");
        }
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
            screen(
                r#"<XCUIElementTypeNavigationBar name="World Clock" name="Stopwatch" visible="true"/>"#,
            ),
            screen(r#"<XCUIElementTypeStaticText label="&unknown;" visible="true"/>"#),
            screen("<![CDATA[private-fixture-content]]>"),
            format!("{}{}", "<node>".repeat(130), "</node>".repeat(130)),
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
