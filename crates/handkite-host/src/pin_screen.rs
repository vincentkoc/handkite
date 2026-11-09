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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseError;

struct Node {
    kind: String,
    name: Option<String>,
    value: Option<String>,
    bundle_id: Option<String>,
    point: Option<Point>,
    size: Option<(f64, f64)>,
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
            _ => {}
        }
    }
    let point = match (x, y, width, height) {
        (Some(x), Some(y), Some(width), Some(height))
            if [x, y, width, height].iter().all(|value| value.is_finite())
                && width > 0.0
                && height > 0.0
                && width <= 2000.0
                && height <= 2000.0 =>
        {
            Some(Point {
                x: x + width / 2.0,
                y: y + height / 2.0,
            })
        }
        _ => None,
    };
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
        size,
    })
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= ALIGN_TOLERANCE
}

fn valid_keypad(keys: &[Point; 10]) -> bool {
    let rows = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
    keys.iter()
        .all(|key| (0.0..=10_000.0).contains(&key.x) && (0.0..=10_000.0).contains(&key.y))
        && rows.iter().all(|[left, middle, right]| {
            near(keys[*left].y, keys[*middle].y)
                && near(keys[*middle].y, keys[*right].y)
                && keys[*middle].x - keys[*left].x >= 40.0
                && keys[*right].x - keys[*middle].x >= 40.0
        })
        && (1..=3).all(|column| {
            near(keys[column].x, keys[column + 3].x) && near(keys[column].x, keys[column + 6].x)
        })
        && keys[1].y < keys[4].y
        && keys[4].y < keys[7].y
        && near(keys[0].x, keys[8].x)
        && keys[0].y > keys[8].y
}

pub fn parse(xml: &str) -> Result<ParsedPasscodeView, ParseError> {
    if xml.len() > MAX_XML_BYTES {
        return Err(ParseError);
    }
    let mut reader = Reader::from_str(xml);
    let mut keys = [None; 10];
    let mut field = None;
    let mut field_seen = false;
    let mut buttons = Vec::new();
    let mut lock_marker = false;
    let mut screen_size = None;
    let mut foreground_app = None;
    let mut home_icons = 0_usize;
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
                    field = node.value;
                } else if node.kind == "XCUIElementTypeKey" {
                    if let (Some(name), Some(point)) = (node.name, node.point) {
                        if name.len() == 1 && name.as_bytes()[0].is_ascii_digit() {
                            let digit = usize::from(name.as_bytes()[0] - b'0');
                            if keys[digit].replace(point).is_some() {
                                return Err(ParseError);
                            }
                        }
                    }
                } else if node.kind == "XCUIElementTypeButton" {
                    if let Some(point) = node.point {
                        buttons.push(point);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let keypad = keys
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .and_then(|keys| keys.try_into().ok())
        .filter(valid_keypad);
    let delete_key = keypad.as_ref().and_then(|keys: &[Point; 10]| {
        buttons.into_iter().find(|button| {
            near(button.x, keys[9].x) && button.y > keys[9].y + 50.0 && near(button.y, keys[0].y)
        })
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let mut xml = String::from(
            "<XCUIElementTypeApplication><XCUIElementTypeOther name=\"passcode-view\"/>",
        );
        for digit in 0..=9 {
            let (column, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!("<XCUIElementTypeKey name=\"{digit}\" x=\"{}\" y=\"{}\" width=\"80\" height=\"80\"/>", 40 + column * 100, 300 + row * 100));
        }
        xml.push_str("<XCUIElementTypeSecureTextField value=\"0 of 6 digits\"/><XCUIElementTypeButton x=\"240\" y=\"600\" width=\"80\" height=\"80\"/></XCUIElementTypeApplication>");
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
    fn refuses_duplicate_or_rotated_keypad() {
        let duplicate = fixture().replace(
            "</XCUIElementTypeApplication>",
            "<XCUIElementTypeKey name=\"1\" x=\"40\" y=\"300\" width=\"80\" height=\"80\"/></XCUIElementTypeApplication>",
        );
        assert!(parse(&duplicate).is_err());
        let rotated = fixture().replace(
            "name=\"2\" x=\"140\" y=\"300\"",
            "name=\"2\" x=\"140\" y=\"500\"",
        );
        assert!(parse(&rotated).unwrap().keypad.is_none());
    }
}
