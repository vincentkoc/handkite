use handkite_core::protocol::{LockState, Point};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const ALIGN_TOLERANCE: f64 = 12.0;

pub struct ParsedPasscodeView {
    pub visible_lock: LockState,
    pub screen_size: Option<(f64, f64)>,
    pub secure_field_value: Option<String>,
    pub keypad: Option<[Point; 10]>,
    pub delete_key: Option<Point>,
    pub has_alert: bool,
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
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

struct Node {
    kind: String,
    name: Option<String>,
    value: Option<String>,
    bundle_id: Option<String>,
    point: Option<Point>,
    rect: Option<Rect>,
    size: Option<(f64, f64)>,
    visible: Option<bool>,
    enabled: Option<bool>,
}

fn node(reader: &Reader<&[u8]>, element: &BytesStart<'_>) -> Result<Node, ParseError> {
    let kind = std::str::from_utf8(element.name().as_ref())
        .map_err(|_| ParseError)?
        .to_owned();
    let mut name = None;
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
            b"name" => name = Some(decoded.into_owned()),
            b"value" => value = Some(decoded.into_owned()),
            b"bundleId" => bundle_id = Some(decoded.into_owned()),
            b"x" => x = decoded.parse::<f64>().ok(),
            b"y" => y = decoded.parse::<f64>().ok(),
            b"width" => width = decoded.parse::<f64>().ok(),
            b"height" => height = decoded.parse::<f64>().ok(),
            b"visible" => visible = Some(decoded == "true"),
            b"enabled" => enabled = Some(decoded == "true"),
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
    let mut buttons = Vec::new();
    let mut lock_marker = false;
    let mut screen_size = None;
    let mut foreground_app = None;
    let mut home_icons = 0_usize;
    let mut has_alert = false;
    loop {
        match reader.read_event().map_err(|_| ParseError)? {
            Event::Start(element) | Event::Empty(element) => {
                let node = node(&reader, &element)?;
                if node.kind == "XCUIElementTypeApplication" && screen_size.is_none() {
                    screen_size = node.size;
                    foreground_app = node.bundle_id.clone();
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
                if node.kind == "XCUIElementTypeSecureTextField" {
                    if field_seen {
                        return Err(ParseError);
                    }
                    field_seen = true;
                    if node.visible == Some(true)
                        && node.enabled == Some(true)
                        && node.rect.is_some_and(|rect| {
                            screen_size.is_some_and(|screen| rect.inside(screen))
                        })
                    {
                        field = node.value;
                    }
                } else if node.kind == "XCUIElementTypeKey" {
                    if let Some(name) = node.name {
                        if name.len() == 1 && name.as_bytes()[0].is_ascii_digit() {
                            let digit = usize::from(name.as_bytes()[0] - b'0');
                            if seen_digit[digit] {
                                return Err(ParseError);
                            }
                            seen_digit[digit] = true;
                            if node.visible == Some(true) && node.enabled == Some(true) {
                                if let (Some(point), Some(rect)) = (node.point, node.rect) {
                                    keys[digit] = Some((point, rect));
                                }
                            }
                        }
                    }
                } else if node.kind == "XCUIElementTypeButton"
                    && node.visible == Some(true)
                    && node.enabled == Some(true)
                {
                    if let (Some(point), Some(rect)) = (node.point, node.rect) {
                        buttons.push((point, rect));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let key_rects = keys
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .and_then(|keys| keys.try_into().ok())
        .filter(|keys| screen_size.is_some_and(|screen| valid_keypad(keys, screen)));
    let keypad = key_rects
        .as_ref()
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
        secure_field_value: field,
        keypad,
        delete_key,
        has_alert,
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
}
