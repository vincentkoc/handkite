use handkite_core::protocol::Point;
use quick_xml::events::Event;
use std::collections::BTreeMap;

type Attributes = BTreeMap<String, String>;

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
    parse(xml, Some(at)) == Some(())
}

pub(crate) fn safe_context(xml: &str) -> bool {
    parse(xml, None) == Some(())
}

fn parse(xml: &str, at: Option<Point>) -> Option<()> {
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
                if matches!(
                    kind.as_str(),
                    "XCUIElementTypeAlert"
                        | "XCUIElementTypeSheet"
                        | "XCUIElementTypeSecureTextField"
                ) {
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
                    screen = Some(rect);
                }
                if kind == "XCUIElementTypeButton"
                    && attrs.get("visible").map(String::as_str) != Some("false")
                    && let Some(at) = at
                {
                    let rect = Rect::read(&attrs)?;
                    if rect.contains(at) {
                        if target
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
                    && (at.is_none() || target))
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
