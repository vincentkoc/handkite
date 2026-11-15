use handkite_core::protocol::{ButtonTarget, Point};
use quick_xml::events::Event;
use std::collections::BTreeMap;

pub(crate) type Attributes = BTreeMap<String, String>;

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn read(attrs: &Attributes) -> Option<Self> {
        let number = |key| {
            attrs
                .get(key)?
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
        };
        let rect = Self {
            x: number("x")?,
            y: number("y")?,
            width: number("width")?,
            height: number("height")?,
        };
        (rect.x >= 0.0
            && rect.y >= 0.0
            && rect.width > 0.0
            && rect.height > 0.0
            && rect.x + rect.width <= 10_000.0
            && rect.y + rect.height <= 10_000.0)
            .then_some(rect)
    }

    fn contains(self, at: Point) -> bool {
        at.x >= self.x
            && at.y >= self.y
            && at.x < self.x + self.width
            && at.y < self.y + self.height
    }

    fn encloses(self, other: Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }
}

/// Proves one unambiguous public button at this point in the complete tree.
/// The caller checks native lock, ownership and capture freshness separately.
pub(crate) fn visible_button(xml: &str, at: Point) -> bool {
    parse(xml, Target::Button(at)) == Some(())
}

pub(crate) fn named_button(xml: &str, target: &ButtonTarget) -> bool {
    target.valid() && parse(xml, Target::NamedButton(target)) == Some(())
}

pub(crate) fn clickable_kind(kind: &str) -> bool {
    matches!(kind, "XCUIElementTypeButton" | "XCUIElementTypeMenuItem")
}

pub(crate) fn visible_modal_button(xml: &str, at: Point) -> bool {
    parse(xml, Target::ModalButton(at)) == Some(())
}

pub(crate) fn safe_context(xml: &str) -> bool {
    parse(xml, Target::Context) == Some(())
}

pub(crate) fn swipe_context(xml: &str, from: Point, to: Point) -> bool {
    [from.x, from.y, to.x, to.y]
        .iter()
        .all(|value| value.is_finite())
        && parse(xml, Target::SwipeContext { from, to }) == Some(())
}

pub(crate) fn visible_text_input(xml: &str) -> bool {
    parse(xml, Target::TextInput) == Some(())
}

/// Uses the current iOS selection; missing focus metadata is not focus proof.
pub(crate) fn visible_current_text_input(xml: &str) -> bool {
    parse(xml, Target::CurrentTextInput) == Some(())
}

/// Extract only after the complete baseline passed the existing text guards.
pub(crate) fn current_text_snapshot(xml: &str) -> Option<(Attributes, Attributes)> {
    if !visible_current_text_input(xml) {
        return None;
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut screen = None;
    let mut field = None;
    loop {
        match reader.read_event().ok()? {
            Event::Start(node) | Event::Empty(node) => {
                let kind = std::str::from_utf8(node.name().as_ref()).ok()?.to_owned();
                if kind != "XCUIElementTypeApplication"
                    && !matches!(
                        kind.as_str(),
                        "XCUIElementTypeTextField"
                            | "XCUIElementTypeTextView"
                            | "XCUIElementTypeSearchField"
                    )
                {
                    continue;
                }
                let mut attrs = Attributes::new();
                for attr in node.attributes() {
                    let attr = attr.ok()?;
                    attrs.insert(
                        std::str::from_utf8(attr.key.as_ref()).ok()?.into(),
                        attr.decode_and_unescape_value(reader.decoder())
                            .ok()?
                            .into_owned(),
                    );
                }
                attrs.insert("type".into(), kind.clone());
                if kind == "XCUIElementTypeApplication" {
                    screen = Some(attrs);
                } else if attrs.get("visible").map(String::as_str) == Some("true")
                    && attrs.get("enabled").map(String::as_str) == Some("true")
                {
                    field = Some(attrs);
                }
            }
            Event::Eof => return Some((screen?, field?)),
            _ => {}
        }
    }
}

pub(crate) fn visible_text_target(xml: &str, at: Point) -> bool {
    parse(xml, Target::TextPoint(at)) == Some(())
}

/// Focuses one public text view; does not authorize text or selection input.
pub(crate) fn visible_text_focus_target(xml: &str, at: Point) -> bool {
    parse(xml, Target::TextFocusPoint(at)) == Some(())
}

enum Target<'a> {
    Context,
    SwipeContext { from: Point, to: Point },
    Button(Point),
    ModalButton(Point),
    TextInput,
    CurrentTextInput,
    TextPoint(Point),
    TextFocusPoint(Point),
    NamedButton(&'a ButtonTarget),
}

fn parse(xml: &str, target_kind: Target<'_>) -> Option<()> {
    let at = match target_kind {
        Target::Button(at)
        | Target::ModalButton(at)
        | Target::TextPoint(at)
        | Target::TextFocusPoint(at) => Some(at),
        _ => None,
    };
    let allow_modal = matches!(target_kind, Target::ModalButton(_));
    let current_text = matches!(target_kind, Target::CurrentTextInput | Target::TextPoint(_));
    let focus_text = matches!(target_kind, Target::TextFocusPoint(_));
    let text_point = matches!(
        target_kind,
        Target::TextPoint(_) | Target::TextFocusPoint(_)
    );
    let require_text = current_text || focus_text || matches!(target_kind, Target::TextInput);
    if xml.is_empty()
        || xml.len() > 2 * 1024 * 1024
        || at.is_some_and(|at| !at.x.is_finite() || !at.y.is_finite())
    {
        return None;
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut roots = 0;
    let mut declared = false;
    let mut screen = None;
    let mut target = false;
    let mut modal = None;
    let mut target_in_modal = false;
    let mut text_inputs = 0;
    let mut keyboards = 0;
    loop {
        let event = reader.read_event().ok()?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() >= 128 {
                    return None;
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots != 1 {
                        return None;
                    }
                }
                let kind = std::str::from_utf8(node.name().as_ref()).ok()?.to_owned();
                if matches!(kind.as_str(), "XCUIElementTypeSecureTextField") {
                    return None;
                }
                let mut attrs = Attributes::new();
                for attr in node.attributes() {
                    let attr = attr.ok()?;
                    let key = std::str::from_utf8(attr.key.as_ref()).ok()?.to_owned();
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .ok()?
                        .into_owned();
                    if [
                        "proud-lock",
                        "passcode-view",
                        "call-to-action-label",
                        "Enter Passcode",
                    ]
                    .iter()
                    .any(|marker| value.contains(marker))
                    {
                        return None;
                    }
                    if attrs.insert(key, value).is_some() {
                        return None;
                    }
                }
                if attrs.get("type").is_some_and(|value| value != &kind) {
                    return None;
                }
                if kind == "XCUIElementTypeApplication" {
                    if screen.is_some() {
                        return None;
                    }
                    let rect = Rect::read(&attrs)?;
                    if rect.x != 0.0 || rect.y != 0.0 || at.is_some_and(|at| !rect.contains(at)) {
                        return None;
                    }
                    if let Target::SwipeContext { from, to } = target_kind
                        && (!rect.contains(from) || !rect.contains(to))
                    {
                        return None;
                    }
                    screen = Some(rect);
                }
                if matches!(
                    kind.as_str(),
                    "XCUIElementTypeAlert" | "XCUIElementTypeSheet"
                ) {
                    if !allow_modal
                        || modal.is_some()
                        || attrs.get("visible").map(String::as_str) != Some("true")
                        || attrs.get("enabled").map(String::as_str) != Some("true")
                        || !stack
                            .iter()
                            .any(|parent| parent == "XCUIElementTypeApplication")
                    {
                        return None;
                    }
                    let rect = Rect::read(&attrs)?;
                    if !screen?.encloses(rect) {
                        return None;
                    }
                    modal = Some(rect);
                }
                let text_kind = matches!(
                    kind.as_str(),
                    "XCUIElementTypeSearchField"
                        | "XCUIElementTypeTextField"
                        | "XCUIElementTypeTextView"
                        | "XCUIElementTypeKeyboard"
                );
                if focus_text
                    && text_kind
                    && attrs.get("visible").map(String::as_str) != Some("false")
                    && (kind != "XCUIElementTypeTextView"
                        || attrs.get("visible").map(String::as_str) != Some("true")
                        || attrs.get("enabled").map(String::as_str) != Some("true"))
                {
                    return None;
                }
                if require_text
                    && text_kind
                    && attrs.get("visible").map(String::as_str) != Some("false")
                    && attrs.get("enabled").map(String::as_str) != Some("false")
                {
                    let rect = Rect::read(&attrs)?;
                    if attrs.get("visible").map(String::as_str) != Some("true")
                        || attrs.get("enabled").map(String::as_str) != Some("true")
                        || !focus_text
                            && kind != "XCUIElementTypeKeyboard"
                            && attrs.get("focused").map(String::as_str) == Some("false")
                        || (current_text || focus_text)
                            && attrs
                                .get("focused")
                                .is_some_and(|value| !matches!(value.as_str(), "true" | "false"))
                        || kind == "XCUIElementTypeTextView" && !current_text && !focus_text
                        || !stack
                            .iter()
                            .any(|parent| parent == "XCUIElementTypeApplication")
                        || !screen?.encloses(rect)
                    {
                        return None;
                    }
                    if kind == "XCUIElementTypeKeyboard" {
                        if at.is_some_and(|point| rect.contains(point)) {
                            return None;
                        }
                        keyboards += 1;
                    } else {
                        text_inputs += 1;
                        if text_point && rect.contains(at?) {
                            target = true;
                        }
                    }
                }
                if clickable_kind(&kind)
                    && let Target::NamedButton(expected) = &target_kind
                    && attrs.get("label") == Some(&expected.label)
                    && attrs.get("visible").map(String::as_str) != Some("false")
                {
                    let rect = Rect::read(&attrs)?;
                    if target
                        || attrs.get("visible").map(String::as_str) != Some("true")
                        || attrs.get("enabled").map(String::as_str) != Some("true")
                        || !screen?.encloses(rect)
                        || rect.x != expected.x
                        || rect.y != expected.y
                        || rect.width != expected.width
                        || rect.height != expected.height
                    {
                        return None;
                    }
                    target = true;
                }
                if (kind == "XCUIElementTypeButton"
                    || text_point && kind == "XCUIElementTypeMenuItem")
                    && attrs.get("visible").map(String::as_str) != Some("false")
                    && let Some(at) = at
                {
                    let rect = Rect::read(&attrs)?;
                    if rect.contains(at) {
                        if text_point
                            || target
                            || !stack
                                .iter()
                                .any(|parent| parent == "XCUIElementTypeApplication")
                            || !screen?.encloses(rect)
                            || attrs.get("visible").map(String::as_str) != Some("true")
                            || attrs.get("enabled").map(String::as_str) != Some("true")
                            || attrs
                                .get("label")
                                .is_none_or(|label| label.trim().is_empty())
                        {
                            return None;
                        }
                        target = true;
                        target_in_modal = stack.iter().any(|parent| {
                            parent == "XCUIElementTypeAlert" || parent == "XCUIElementTypeSheet"
                        }) && modal.is_some_and(|modal| modal.encloses(rect));
                    }
                }
                if is_start {
                    stack.push(kind);
                }
            }
            Event::End(_) => {
                stack.pop()?;
            }
            Event::Text(text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Event::Decl(_) if roots == 0 && !declared => {
                declared = true;
            }
            Event::Eof => {
                return (roots == 1
                    && stack.is_empty()
                    && screen.is_some()
                    && (at.is_none() && !matches!(target_kind, Target::NamedButton(_)) || target)
                    && (!require_text
                        || text_inputs == 1 && keyboards == usize::from(!focus_text))
                    && (!allow_modal || modal.is_some() && target_in_modal))
                    .then_some(());
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BUTTON: &str = r#"<XCUIElementTypeButton label="Clear text" visible="true" enabled="true" x="270" y="490" width="30" height="30"/>"#;
    fn tree(buttons: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="393" height="852">{buttons}</XCUIElementTypeApplication></AppiumAUT>"#
        )
    }
    fn point() -> Point {
        Point { x: 285.0, y: 505.0 }
    }

    #[test]
    fn swipe_context_requires_both_points_inside_an_unobstructed_safe_app() {
        let from = Point { x: 195.0, y: 650.0 };
        let to = Point { x: 195.0, y: 250.0 };
        assert!(swipe_context(&tree(""), from, to));
        for bad in [
            Point { x: -1.0, y: 200.0 },
            Point { x: 393.0, y: 200.0 },
            Point {
                x: f64::NAN,
                y: 200.0,
            },
            Point { x: 200.0, y: 852.0 },
        ] {
            assert!(!swipe_context(&tree(""), bad, to));
            assert!(!swipe_context(&tree(""), from, bad));
        }
        for body in [
            "<XCUIElementTypeSecureTextField visible=\"false\"/>",
            "<XCUIElementTypeAlert/>",
            "<XCUIElementTypeSheet/>",
            "<XCUIElementTypeOther label=\"Enter Passcode\"/>",
            "<broken>",
        ] {
            assert!(!swipe_context(&tree(body), from, to), "{body}");
        }
    }

    #[test]
    fn named_button_disambiguates_overlap_without_relaxing_coordinate_taps() {
        let target = ButtonTarget {
            label: "Clear text".into(),
            x: 270.0,
            y: 490.0,
            width: 30.0,
            height: 30.0,
        };
        let under = r#"<XCUIElementTypeButton label="Underlying row" visible="true" enabled="true" x="0" y="488" width="393" height="62"/>"#;
        let xml = tree(&format!("{under}{BUTTON}"));
        assert!(!visible_button(&xml, point()));
        assert!(named_button(&xml, &target));
        for body in [
            format!("{BUTTON}{BUTTON}"),
            BUTTON.replace("490", "491"),
            BUTTON.replace("enabled=\"true\"", "enabled=\"false\""),
            format!("<XCUIElementTypeSheet>{BUTTON}</XCUIElementTypeSheet>"),
            format!("{BUTTON}<XCUIElementTypeSecureTextField/>"),
        ] {
            assert!(!named_button(&tree(&body), &target));
        }
    }

    #[test]
    fn named_menu_item_keeps_identity_and_ambiguity_checks() {
        let target = ButtonTarget {
            label: "Clear text".into(),
            x: 270.0,
            y: 490.0,
            width: 30.0,
            height: 30.0,
        };
        let menu = BUTTON.replace("XCUIElementTypeButton", "XCUIElementTypeMenuItem");
        assert!(named_button(&tree(&menu), &target));
        for body in [
            format!("{menu}{BUTTON}"),
            format!("{menu}{menu}"),
            menu.replace("490", "491"),
            menu.replace("enabled=\"true\"", "enabled=\"false\""),
            menu.replace("visible=\"true\"", "visible=\"false\""),
            menu.replace("MenuItem", "StaticText"),
        ] {
            assert!(!named_button(&tree(&body), &target));
        }
    }

    #[test]
    fn text_input_requires_one_editable_field_and_keyboard() {
        let field = r#"<XCUIElementTypeSearchField visible="true" enabled="true" x="20" y="67" width="298" height="44"/>"#;
        let keyboard = r#"<XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/>"#;
        let body = format!("{field}{keyboard}");
        assert!(visible_text_input(&tree(&body)));
        assert!(visible_text_input(&tree(
            &body.replace("SearchField", "TextField")
        )));
        assert!(visible_text_input(&tree(&format!(
            "{body}<XCUIElementTypeTextView visible=\"true\" enabled=\"false\"/>"
        ))));
        for invalid in [
            field.to_owned(),
            keyboard.to_owned(),
            format!("{body}{field}"),
            format!("{body}{keyboard}"),
            body.replace("SearchField", "SecureTextField"),
            body.replace("SearchField", "TextView"),
            body.replacen("enabled=\"true\"", "enabled=\"false\"", 1),
            body.replacen("enabled=\"true\"", "", 1),
            body.replacen("visible=\"true\"", "", 1),
            body.replacen("SearchField ", "SearchField focused=\"false\" ", 1),
            body.replace("width=\"298\"", "width=\"999\""),
            format!("{body}<XCUIElementTypeTextView visible=\"true\"/>"),
            format!("{body}<XCUIElementTypeSecureTextField visible=\"false\"/>"),
            format!("{body}<XCUIElementTypeAlert/>"),
            format!("{body}<XCUIElementTypeOther label=\"Enter Passcode\"/>"),
            format!("{body}<broken>"),
        ] {
            assert!(!visible_text_input(&tree(&invalid)), "{invalid}");
        }
    }

    #[test]
    fn text_gesture_requires_an_unobstructed_point_inside_the_only_field() {
        let field = r#"<XCUIElementTypeSearchField visible="true" enabled="true" x="20" y="67" width="298" height="44"/>"#;
        let keyboard = r#"<XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/>"#;
        let at = Point { x: 80.0, y: 80.0 };
        let body = format!("{field}{keyboard}");
        assert!(visible_text_target(&tree(&body), at));
        for point in [
            Point { x: 19.0, y: 80.0 },
            Point { x: 318.0, y: 80.0 },
            Point { x: 80.0, y: 550.0 },
        ] {
            assert!(!visible_text_target(&tree(&body), point));
        }
        for extra in [
            field,
            r#"<XCUIElementTypeButton label="Clear" visible="true" enabled="true" x="60" y="70" width="40" height="30"/>"#,
            r#"<XCUIElementTypeMenuItem label="Select" visible="true" enabled="true" x="60" y="70" width="40" height="30"/>"#,
            r#"<XCUIElementTypeMenuItem label="Select" enabled="true" x="60" y="70" width="40" height="30"/>"#,
            r#"<XCUIElementTypeAlert/>"#,
        ] {
            assert!(!visible_text_target(&tree(&format!("{body}{extra}")), at));
        }
    }

    #[test]
    fn current_selection_and_gestures_accept_one_text_view_without_asserting_focus() {
        let editor = r#"<XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="544"/>"#;
        let keyboard = r#"<XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/>"#;
        let body = format!("{editor}{keyboard}");
        let at = Point { x: 80.0, y: 40.0 };
        for focus in ["", " focused=\"true\""] {
            let xml = tree(&body.replacen("TextView ", &format!("TextView{focus} "), 1));
            assert!(visible_current_text_input(&xml));
            assert!(visible_text_target(&xml, at));
            assert!(!visible_text_input(&xml));
        }
        for invalid in [
            editor.to_owned(),
            format!("{body}{editor}"),
            format!("{body}{keyboard}"),
            format!("{body}{}", editor.replace("TextView", "SearchField")),
            body.replacen("enabled=\"true\"", "enabled=\"false\"", 1),
            body.replacen("enabled=\"true\"", "", 1),
            body.replacen("visible=\"true\"", "", 1),
            body.replacen("TextView ", "TextView focused=\"false\" ", 1),
            body.replacen("TextView ", "TextView focused=\"unknown\" ", 1),
            body.replace("width=\"393\"", "width=\"999\""),
            format!("{body}<XCUIElementTypeSecureTextField visible=\"false\"/>"),
            format!("{body}<XCUIElementTypeAlert/>"),
            format!("{body}<XCUIElementTypeOther label=\"Enter Passcode\"/>"),
            format!("{body}<broken>"),
        ] {
            assert!(!visible_current_text_input(&tree(&invalid)), "{invalid}");
            assert!(!visible_text_target(&tree(&invalid), at), "{invalid}");
        }
    }

    #[test]
    fn focus_tap_requires_one_public_reading_view_without_keyboard_or_overlap() {
        let view = r#"<XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="852"/>"#;
        let at = Point { x: 55.0, y: 136.0 };
        for focused in ["", " focused=\"false\"", " focused=\"true\""] {
            let xml = tree(&view.replace("TextView ", &format!("TextView{focused} ")));
            assert!(visible_text_focus_target(&xml, at));
            assert!(!visible_current_text_input(&xml));
            assert!(!visible_text_target(&xml, at));
        }
        for point in [
            Point { x: -1.0, y: 136.0 },
            Point { x: 393.0, y: 136.0 },
            Point { x: 55.0, y: 852.0 },
            Point {
                x: f64::NAN,
                y: 136.0,
            },
        ] {
            assert!(!visible_text_focus_target(&tree(view), point));
        }
        for extra in [
            view,
            r#"<XCUIElementTypeTextView enabled="true" x="0" y="0" width="393" height="852"/>"#,
            r#"<XCUIElementTypeTextField/>"#,
            r#"<XCUIElementTypeKeyboard visible="true" enabled="false"/>"#,
            r#"<XCUIElementTypeKeyboard/>"#,
            r#"<XCUIElementTypeButton label="Overlay" visible="true" enabled="true" x="0" y="0" width="100" height="200"/>"#,
            r#"<XCUIElementTypeMenuItem label="Overlay" enabled="true" x="0" y="0" width="100" height="200"/>"#,
            r#"<XCUIElementTypeSecureTextField visible="false"/>"#,
            r#"<XCUIElementTypeAlert/>"#,
            r#"<XCUIElementTypeSheet/>"#,
            r#"<XCUIElementTypeOther label="Enter Passcode"/>"#,
            "<broken>",
        ] {
            assert!(
                !visible_text_focus_target(&tree(&format!("{view}{extra}")), at),
                "{extra}"
            );
        }
        for invalid in [
            view.replace("enabled=\"true\"", "enabled=\"false\""),
            view.replace("visible=\"true\"", ""),
            view.replace("TextView ", "TextView focused=\"unknown\" "),
            view.replace("width=\"393\"", "width=\"500\""),
        ] {
            assert!(!visible_text_focus_target(&tree(&invalid), at));
        }
    }

    #[test]
    fn text_view_gesture_refuses_keyboard_overlap_and_editor_edges() {
        let editor = r#"<XCUIElementTypeTextView visible="true" enabled="true" x="0" y="0" width="393" height="600"/>"#;
        let keyboard = r#"<XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="550" width="393" height="302"/>"#;
        let xml = tree(&format!("{editor}{keyboard}"));
        assert!(visible_text_target(&xml, Point { x: 50.0, y: 549.0 }));
        for point in [
            Point { x: 50.0, y: 550.0 },
            Point { x: 393.0, y: 50.0 },
            Point { x: 50.0, y: 600.0 },
        ] {
            assert!(!visible_text_target(&xml, point));
        }
        let button = r#"<XCUIElementTypeButton label="Overlay" visible="true" enabled="true" x="20" y="20" width="100" height="100"/>"#;
        assert!(!visible_text_target(
            &tree(&format!("{editor}{keyboard}{button}")),
            Point { x: 50.0, y: 50.0 }
        ));
    }

    fn modal(kind: &str, body: &str) -> String {
        format!(
            r#"<{kind} visible="true" enabled="true" x="200" y="400" width="120" height="200">{body}</{kind}>"#
        )
    }

    #[test]
    fn modal_tap_requires_one_visible_modal_and_an_enclosed_button() {
        for kind in ["XCUIElementTypeAlert", "XCUIElementTypeSheet"] {
            let body = modal(kind, BUTTON);
            assert!(visible_modal_button(&tree(&body), point()));
            assert!(!visible_button(&tree(&body), point()));
            assert!(!safe_context(&tree(&body)));
            for changed in [
                modal(kind, "") + BUTTON,
                body.repeat(2),
                modal(kind, &body),
                body.replace("visible=\"true\"", "visible=\"false\""),
                body.replace("enabled=\"true\"", "enabled=\"false\""),
                body.replace("width=\"120\"", "width=\"NaN\""),
                body.replace("width=\"120\"", "width=\"300\""),
                body.replace("width=\"120\"", "width=\"80\""),
                modal(kind, &BUTTON.repeat(2)),
                modal(kind, &BUTTON.replace("label=\"Clear text\"", "label=\" \"")),
                body.clone() + "<XCUIElementTypeSecureTextField/>",
                body.clone() + "<node name=\"passcode-view\"/>",
                body.clone() + "<node name=\"proud-lock\"/>",
                body.clone() + "<node label=\"Enter Passcode\"/>",
                body.clone() + "<node name=\"call-to-action-label\"/>",
            ] {
                assert!(!visible_modal_button(&tree(&changed), point()), "{changed}");
            }
        }
        assert!(!visible_modal_button(&tree(BUTTON), point()));
    }

    #[test]
    fn admits_only_one_visible_enabled_button_containing_the_exact_point() {
        assert!(visible_button(&tree(BUTTON), point()));
        assert!(visible_button(
            &tree(&BUTTON.replace("label=", "type=\"XCUIElementTypeButton\" label=")),
            point()
        ));
        assert!(!visible_button(
            &tree(&BUTTON.replace("label=", "type=\"XCUIElementTypeSecureTextField\" label=")),
            point()
        ));
        assert!(!visible_button(&tree(BUTTON), Point { x: 300.0, y: 505.0 }));
        assert!(!visible_button(
            &tree(BUTTON),
            Point {
                x: f64::NAN,
                y: 505.0
            }
        ));
        assert!(!visible_button(&tree(&BUTTON.repeat(2)), point()));
        for (old, new) in [
            ("visible=\"true\"", "visible=\"false\""),
            ("visible=\"true\"", ""),
            ("enabled=\"true\"", "enabled=\"false\""),
            ("enabled=\"true\"", ""),
            ("label=\"Clear text\"", "label=\" \""),
            ("label=\"Clear text\"", ""),
            ("width=\"30\"", "width=\"NaN\""),
            ("width=\"30\"", "width=\"200\""),
            ("x=\"270\"", "x=\"-1\""),
        ] {
            assert!(
                !visible_button(&tree(&BUTTON.replace(old, new)), point()),
                "{old} -> {new}"
            );
        }
        for extra in [
            BUTTON.to_owned(),
            BUTTON.replace("enabled=\"true\"", "enabled=\"false\""),
            BUTTON.replace("label=\"Clear text\"", ""),
        ] {
            assert!(!visible_button(&tree(&format!("{BUTTON}{extra}")), point()));
        }
        assert!(visible_button(
            &tree(&format!(
                "{BUTTON}{}",
                BUTTON.replace("visible=\"true\"", "visible=\"false\"")
            )),
            point()
        ));
    }

    #[test]
    fn rejects_unsafe_or_malformed_context_anywhere_in_the_tree() {
        for extra in [
            "<XCUIElementTypeAlert/>",
            "<XCUIElementTypeSheet/>",
            "<XCUIElementTypeSecureTextField/>",
            "<node label=\"Enter Passcod&#101;\"/>",
            "<node name=\"proud-lock\"/>",
            "<node name=\"passcode-view\"/>",
            "<node name=\"call-to-action-label\"/>",
            "<XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"393\" height=\"852\"/>",
            "<node>",
            "<!DOCTYPE x>",
            "<!-- ambiguous -->",
            "<wrong></other>",
        ] {
            assert!(!safe_context(&tree(&format!("{BUTTON}{extra}"))), "{extra}");
            assert!(
                !visible_button(&tree(&format!("{BUTTON}{extra}")), point()),
                "{extra}"
            );
        }
        assert!(!visible_button(&(tree(BUTTON) + "<other/>"), point()));
        assert!(!visible_button(
            &tree(BUTTON).replace("label=", "enabled=\"true\" label="),
            point()
        ));
        assert!(!visible_button(&format!("<root>{BUTTON}</root>"), point()));
        assert!(!visible_button(&format!("{}x", tree(BUTTON)), point()));
        assert!(!visible_button(
            &tree(BUTTON).replace("<AppiumAUT>", "<?xml version=\"1.0\"?><AppiumAUT>"),
            point()
        ));
    }

    #[test]
    fn home_context_requires_one_valid_application_but_not_a_button() {
        assert!(safe_context(&tree("")));
        assert!(safe_context(&tree(BUTTON)));
        for xml in [
            "<root/>",
            "<root><node/></root>",
            "",
            "<AppiumAUT><XCUIElementTypeApplication/></AppiumAUT>",
        ] {
            assert!(!safe_context(xml));
        }
        assert!(!safe_context(
            &tree("").replace("width=\"393\"", "width=\"NaN\"")
        ));
        assert!(!safe_context(&(tree("") + "<other/>")));
        assert!(!safe_context(&tree("").replace(
            "<AppiumAUT>",
            "<AppiumAUT type=\"XCUIElementTypeAlert\">"
        )));
    }
}
