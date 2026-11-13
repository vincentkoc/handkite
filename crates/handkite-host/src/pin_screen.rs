use handkite_core::protocol::{LockState, Point};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::borrow::Cow;

mod normal_lock;
pub use normal_lock::{NormalLockScreen, normal_lock_screen};

const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const ALIGN_TOLERANCE: f64 = 12.0;
const FRAME_EDGE_TOLERANCE: f64 = 2.0;

pub struct ParsedPasscodeView {
    pub normal_lock: Option<NormalLockScreen>,
    pub visible_lock: LockState,
    pub screen_size: Option<(f64, f64)>,
    pub secure_field_value: Option<String>,
    pub keypad: Option<[Point; 10]>,
    pub delete_key: Option<Point>,
    pub has_alert: bool,
    pub digit_keys_seen: usize,
    pub digit_keys_eligible: usize,
    pub digit_keys_visible: usize,
    pub digit_keys_explicitly_hidden: usize,
    pub digit_keys_enabled: usize,
    pub field_seen: bool,
    pub field_visible: bool,
    pub field_explicitly_hidden: bool,
    pub field_enabled: bool,
    pub layout_valid: bool,
    pub visibility_fallback: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseError;

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn center(self) -> Point {
        Point {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }

    fn inside(self, screen: (f64, f64)) -> bool {
        self.x >= 0.0
            && self.y >= 0.0
            && self.x + self.width <= screen.0
            && self.y + self.height <= screen.1
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.width - FRAME_EDGE_TOLERANCE
            && self.x + self.width > other.x + FRAME_EDGE_TOLERANCE
            && self.y < other.y + other.height - FRAME_EDGE_TOLERANCE
            && self.y + self.height > other.y + FRAME_EDGE_TOLERANCE
    }
}

struct Node<'a> {
    kind: &'a str,
    name: Option<Cow<'a, str>>,
    label: Option<Cow<'a, str>>,
    value: Option<Cow<'a, str>>,
    bundle_id: Option<Cow<'a, str>>,
    point: Option<Point>,
    rect: Option<Rect>,
    size: Option<(f64, f64)>,
    visible: Option<bool>,
    enabled: Option<bool>,
}

fn node<'a>(reader: &Reader<&[u8]>, element: &'a BytesStart<'_>) -> Result<Node<'a>, ParseError> {
    let kind = std::str::from_utf8(element.name().into_inner()).map_err(|_| ParseError)?;
    let mut name = None;
    let mut label = None;
    let mut value = None;
    let mut bundle_id = None;
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    let mut visible = None;
    let mut enabled = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ParseError)?;
        let decoded = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|_| ParseError)?;
        match attribute.key.as_ref() {
            b"name" => name = Some(decoded),
            b"label" => label = Some(decoded),
            b"value" => value = Some(decoded),
            b"bundleId" => bundle_id = Some(decoded),
            b"x" => x = decoded.parse::<f64>().ok(),
            b"y" => y = decoded.parse::<f64>().ok(),
            b"width" => width = decoded.parse::<f64>().ok(),
            b"height" => height = decoded.parse::<f64>().ok(),
            b"visible" => {
                visible = match decoded.as_ref() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            b"enabled" => {
                enabled = match decoded.as_ref() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    let rect = match (x, y, width, height) {
        (Some(x), Some(y), Some(width), Some(height))
            if [x, y, width, height].iter().all(|value| value.is_finite())
                && width > 0.0
                && height > 0.0
                && width <= 2000.0
                && height <= 2000.0 =>
        {
            Some(Rect {
                x,
                y,
                width,
                height,
            })
        }
        _ => None,
    };
    let point = rect.map(Rect::center);
    let size = match (width, height) {
        (Some(width), Some(height))
            if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 =>
        {
            Some((width, height))
        }
        _ => None,
    };
    Ok(Node {
        kind,
        name,
        label,
        value,
        bundle_id,
        point,
        rect,
        size,
        visible,
        enabled,
    })
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= ALIGN_TOLERANCE
}

fn valid_keypad(keys: &[(Point, Rect); 10], screen: (f64, f64)) -> bool {
    let rows = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
    keys.iter().all(|key| key.1.inside(screen))
        && (0..10).all(|left| (left + 1..10).all(|right| !keys[left].1.overlaps(keys[right].1)))
        && rows.iter().all(|[left, middle, right]| {
            near(keys[*left].0.y, keys[*middle].0.y)
                && near(keys[*middle].0.y, keys[*right].0.y)
                && keys[*middle].0.x - keys[*left].0.x >= 40.0
                && keys[*right].0.x - keys[*middle].0.x >= 40.0
        })
        && (1..=3).all(|column| {
            near(keys[column].0.x, keys[column + 3].0.x)
                && near(keys[column].0.x, keys[column + 6].0.x)
        })
        && keys[4].0.y - keys[1].0.y >= 40.0
        && keys[7].0.y - keys[4].0.y >= 40.0
        && near(keys[0].0.x, keys[8].0.x)
        && keys[0].0.y - keys[8].0.y >= 40.0
}

pub fn parse(xml: &str) -> Result<ParsedPasscodeView, ParseError> {
    if xml.len() > MAX_XML_BYTES {
        return Err(ParseError);
    }
    let mut reader = Reader::from_str(xml);
    let mut keys = [None; 10];
    let mut seen_digit = [false; 10];
    let mut field = None;
    let mut field_seen = false;
    let mut field_visible = false;
    let mut field_explicitly_hidden = false;
    let mut field_enabled = false;
    let mut buttons = Vec::new();
    let mut lock_marker = false;
    let mut passcode_view_marker = false;
    let mut proud_lock_marker = false;
    let mut passcode_heading_marker = false;
    let mut screen_size = None;
    let mut foreground_app = None;
    let mut home_icons = 0_usize;
    let mut has_alert = false;
    let mut digit_keys_visible = 0_usize;
    let mut digit_keys_explicitly_hidden = 0_usize;
    let mut digit_keys_enabled = 0_usize;
    loop {
        match reader.read_event().map_err(|_| ParseError)? {
            Event::Start(element) | Event::Empty(element) => {
                let node = node(&reader, &element)?;
                if node.kind == "XCUIElementTypeApplication" && screen_size.is_none() {
                    screen_size = node.size;
                    foreground_app = node.bundle_id.map(Cow::into_owned);
                }
                if node.kind == "XCUIElementTypeIcon" {
                    home_icons += 1;
                }
                if node.kind == "XCUIElementTypeAlert" {
                    has_alert = true;
                }
                if node.name.as_deref().is_some_and(|name| {
                    matches!(
                        name,
                        "proud-lock" | "passcode-view" | "call-to-action-label"
                    )
                }) {
                    lock_marker = true;
                }
                if node.kind == "XCUIElementTypeOther"
                    && node.name.as_deref() == Some("passcode-view")
                {
                    passcode_view_marker = true;
                }
                if node.kind == "XCUIElementTypeOther" && node.name.as_deref() == Some("proud-lock")
                {
                    proud_lock_marker = true;
                }
                if node.kind == "XCUIElementTypeStaticText"
                    && node.label.as_deref() == Some("Enter Passcode")
                {
                    passcode_heading_marker = true;
                }
                if node.kind == "XCUIElementTypeSecureTextField" {
                    if field_seen {
                        return Err(ParseError);
                    }
                    field_seen = true;
                    field_visible = node.visible == Some(true);
                    field_explicitly_hidden = node.visible == Some(false);
                    field_enabled = node.enabled == Some(true);
                    if node.enabled == Some(true)
                        && node.rect.is_some_and(|rect| {
                            screen_size.is_some_and(|screen| rect.inside(screen))
                        })
                    {
                        field = node.value.map(Cow::into_owned);
                    }
                } else if node.kind == "XCUIElementTypeKey" {
                    if let Some(name) = node.name
                        && name.len() == 1
                        && name.as_bytes()[0].is_ascii_digit()
                    {
                        let digit = usize::from(name.as_bytes()[0] - b'0');
                        if seen_digit[digit] {
                            return Err(ParseError);
                        }
                        seen_digit[digit] = true;
                        if node.visible == Some(true) {
                            digit_keys_visible += 1;
                        } else if node.visible == Some(false) {
                            digit_keys_explicitly_hidden += 1;
                        }
                        if node.enabled == Some(true) {
                            digit_keys_enabled += 1;
                        }
                        if node.enabled == Some(true)
                            && let (Some(point), Some(rect)) = (node.point, node.rect)
                        {
                            keys[digit] = Some((point, rect));
                        }
                    }
                } else if node.kind == "XCUIElementTypeButton"
                    && node.visible == Some(true)
                    && node.enabled == Some(true)
                    && let (Some(point), Some(rect)) = (node.point, node.rect)
                {
                    buttons.push((point, rect));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let digit_keys_seen = seen_digit.into_iter().filter(|seen| *seen).count();
    let digit_keys_eligible = keys.iter().filter(|key| key.is_some()).count();
    let visibility_fallback = (passcode_view_marker
        || (proud_lock_marker && passcode_heading_marker))
        && !has_alert
        && field_seen
        && field_explicitly_hidden
        && field_enabled
        && digit_keys_seen == 10
        && digit_keys_explicitly_hidden == 10
        && digit_keys_enabled == 10;
    let visibility_admitted = (field_visible && digit_keys_visible == 10) || visibility_fallback;
    let key_rects = keys
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .and_then(|keys| keys.try_into().ok())
        .filter(|keys| screen_size.is_some_and(|screen| valid_keypad(keys, screen)));
    let layout_valid = key_rects.is_some();
    let keypad = key_rects
        .as_ref()
        .filter(|_| visibility_admitted && field.is_some())
        .map(|keys: &[(Point, Rect); 10]| keys.map(|key| key.0));
    let delete_key = keypad.as_ref().and_then(|keys: &[Point; 10]| {
        let candidates: Vec<_> = buttons
            .into_iter()
            .filter(|(button, rect)| {
                screen_size.is_some_and(|screen| rect.inside(screen))
                    && near(button.x, keys[9].x)
                    && button.y > keys[9].y + 50.0
                    && near(button.y, keys[0].y)
            })
            .map(|(button, _)| button)
            .collect();
        (candidates.len() == 1).then(|| candidates[0])
    });
    Ok(ParsedPasscodeView {
        normal_lock: normal_lock_screen(xml).ok(),
        visible_lock: if lock_marker || field.is_some() {
            LockState::Locked
        } else if foreground_app
            .as_deref()
            .is_some_and(|app| app != "com.apple.springboard")
            || home_icons > 0
        {
            LockState::Unlocked
        } else {
            LockState::Uncertain
        },
        screen_size,
        secure_field_value: visibility_admitted.then_some(field).flatten(),
        keypad,
        delete_key,
        has_alert,
        digit_keys_seen,
        digit_keys_eligible,
        digit_keys_visible,
        digit_keys_explicitly_hidden,
        digit_keys_enabled,
        field_seen,
        field_visible,
        field_explicitly_hidden,
        field_enabled,
        layout_valid,
        visibility_fallback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let mut xml = String::from(
            "<XCUIElementTypeApplication width=\"400\" height=\"800\"><XCUIElementTypeOther name=\"passcode-view\"/>",
        );
        for digit in 0..=9 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!("<XCUIElementTypeKey name=\"{digit}\" visible=\"true\" enabled=\"true\" x=\"{}\" y=\"{}\" width=\"80\" height=\"80\"/>", 40 + column * 100, 300 + row * 100));
        }
        xml.push_str("<XCUIElementTypeSecureTextField visible=\"true\" enabled=\"true\" value=\"0 of 6 digits\" x=\"0\" y=\"220\" width=\"400\" height=\"13\"/><XCUIElementTypeButton visible=\"true\" enabled=\"true\" x=\"240\" y=\"600\" width=\"80\" height=\"80\"/></XCUIElementTypeApplication>");
        xml
    }

    #[test]
    fn finds_ten_aligned_keys_and_progress_field() {
        let parsed = parse(&fixture()).unwrap();
        assert_eq!(parsed.visible_lock, LockState::Locked);
        assert_eq!(parsed.secure_field_value.as_deref(), Some("0 of 6 digits"));
        assert!(parsed.keypad.is_some());
        assert!(parsed.delete_key.is_some());
    }

    #[test]
    fn app_or_home_icons_confirm_visible_unlock() {
        assert_eq!(
            parse("<XCUIElementTypeApplication bundleId=\"com.apple.mobiletimer\"/>")
                .unwrap()
                .visible_lock,
            LockState::Unlocked
        );
        assert_eq!(
            parse("<XCUIElementTypeApplication bundleId=\"com.apple.springboard\"><XCUIElementTypeIcon/></XCUIElementTypeApplication>")
                .unwrap()
                .visible_lock,
            LockState::Unlocked
        );
        assert_eq!(
            parse("<XCUIElementTypeApplication bundleId=\"com.apple.springboard\"/>")
                .unwrap()
                .visible_lock,
            LockState::Uncertain
        );
    }

    #[test]
    fn decoded_attributes_keep_keypad_and_field_semantics() {
        let xml = fixture()
            .replace("name=\"1\"", "name=\"&#49;\"")
            .replace("value=\"0 of 6 digits\"", "value=\"0 of &#54; digits\"");
        let parsed = parse(&xml).unwrap();
        assert!(parsed.keypad.is_some());
        assert_eq!(parsed.secure_field_value.as_deref(), Some("0 of 6 digits"));
    }

    #[test]
    fn unused_attributes_still_reject_bad_entities_and_duplicates() {
        for extra in ["unused=\"&unknown;\"", "unused=\"a\" unused=\"b\""] {
            let xml = fixture().replace("name=\"1\"", &format!("name=\"1\" {extra}"));
            assert!(parse(&xml).is_err());
        }
    }

    #[test]
    fn an_alert_requires_manual_inspection() {
        assert!(
            parse(
                "<XCUIElementTypeApplication><XCUIElementTypeAlert/></XCUIElementTypeApplication>"
            )
            .unwrap()
            .has_alert
        );
    }

    #[test]
    fn refuses_duplicate_or_rotated_keypad() {
        let duplicate = fixture().replace(
            "</XCUIElementTypeApplication>",
            "<XCUIElementTypeKey name=\"1\" visible=\"true\" enabled=\"true\" x=\"40\" y=\"300\" width=\"80\" height=\"80\"/></XCUIElementTypeApplication>",
        );
        assert!(parse(&duplicate).is_err());
        let rotated = fixture().replace(
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"140\" y=\"300\"",
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"140\" y=\"500\"",
        );
        assert!(parse(&rotated).unwrap().keypad.is_none());
    }

    #[test]
    fn rejects_hidden_offscreen_or_overlapping_keys() {
        let hidden = fixture().replace(
            "name=\"1\" visible=\"true\"",
            "name=\"1\" visible=\"false\"",
        );
        assert!(parse(&hidden).unwrap().keypad.is_none());
        let offscreen = fixture().replace(
            "name=\"1\" visible=\"true\" enabled=\"true\" x=\"40\"",
            "name=\"1\" visible=\"true\" enabled=\"true\" x=\"1040\"",
        );
        assert!(parse(&offscreen).unwrap().keypad.is_none());
        let overlap = fixture().replace(
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"140\"",
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"100\"",
        );
        assert!(parse(&overlap).unwrap().keypad.is_none());
        let hidden_field = fixture().replace(
            "XCUIElementTypeSecureTextField visible=\"true\"",
            "XCUIElementTypeSecureTextField visible=\"false\"",
        );
        assert!(parse(&hidden_field).unwrap().secure_field_value.is_none());
    }

    #[test]
    fn tolerates_two_pixel_frame_overlap_but_rejects_deep_overlap() {
        let edge_jitter =
            fixture().replace("width=\"80\" height=\"80\"", "width=\"102\" height=\"102\"");
        assert!(parse(&edge_jitter).unwrap().keypad.is_some());
        let deep_overlap = edge_jitter.replace(
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"140\"",
            "name=\"2\" visible=\"true\" enabled=\"true\" x=\"120\"",
        );
        assert!(parse(&deep_overlap).unwrap().keypad.is_none());
    }

    #[test]
    fn admits_only_explicit_systematic_false_visibility() {
        let all_false = fixture().replace("visible=\"true\"", "visible=\"false\"");
        let parsed = parse(&all_false).unwrap();
        assert!(parsed.visibility_fallback);
        assert!(parsed.keypad.is_some());
        assert_eq!(parsed.digit_keys_explicitly_hidden, 10);
        assert!(parsed.field_explicitly_hidden);

        let missing = all_false.replacen("visible=\"false\"", "", 1);
        assert!(!parse(&missing).unwrap().visibility_fallback);
        let malformed = all_false.replacen("visible=\"false\"", "visible=\"unknown\"", 1);
        assert!(!parse(&malformed).unwrap().visibility_fallback);
        let mixed = all_false.replacen("visible=\"false\"", "visible=\"true\"", 1);
        assert!(!parse(&mixed).unwrap().visibility_fallback);
        let disabled = all_false.replacen("enabled=\"true\"", "enabled=\"false\"", 1);
        assert!(!parse(&disabled).unwrap().visibility_fallback);
        let no_marker = all_false.replace("name=\"passcode-view\"", "name=\"other\"");
        assert!(!parse(&no_marker).unwrap().visibility_fallback);
        let alert = all_false.replace(
            "</XCUIElementTypeApplication>",
            "<XCUIElementTypeAlert/></XCUIElementTypeApplication>",
        );
        assert!(!parse(&alert).unwrap().visibility_fallback);
        let offscreen = all_false.replace(
            "name=\"1\" visible=\"false\" enabled=\"true\" x=\"40\"",
            "name=\"1\" visible=\"false\" enabled=\"true\" x=\"1040\"",
        );
        assert!(parse(&offscreen).unwrap().keypad.is_none());
    }

    #[test]
    fn current_ios_lock_markers_require_both_elements() {
        let source = fixture()
            .replace("visible=\"true\"", "visible=\"false\"")
            .replace(
                "<XCUIElementTypeOther name=\"passcode-view\"/>",
                "<XCUIElementTypeOther name=\"proud-lock\"/><XCUIElementTypeStaticText label=\"Enter Passcode\"/>",
            );
        assert!(parse(&source).unwrap().visibility_fallback);
        assert!(
            !parse(&source.replace("name=\"proud-lock\"", "name=\"other\""))
                .unwrap()
                .visibility_fallback
        );
        assert!(
            !parse(&source.replace("label=\"Enter Passcode\"", "label=\"Other\""))
                .unwrap()
                .visibility_fallback
        );
    }
}
