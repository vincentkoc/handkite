use super::ParseError;
use quick_xml::events::Event;

const SPRINGBOARD: &str = "com.apple.springboard";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalLockScreen {
    pub size: (f64, f64),
    pub proud_visible: bool,
}

pub fn normal_lock_screen(xml: &str) -> Result<NormalLockScreen, ParseError> {
    if xml.is_empty() || xml.len() > 2 * 1024 * 1024 {
        return Err(ParseError);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let (mut roots, mut apps, mut proud) = (0, 0, 0);
    let mut size = None;
    let mut proud_visible = None;
    loop {
        let event = reader.read_event().map_err(|_| ParseError)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() >= 128 {
                    return Err(ParseError);
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(ParseError);
                    }
                }
                let (mut name, mut label, mut bundle) = (None, None, None);
                let (mut x, mut y, mut width, mut height) = (None, None, None, None);
                let mut visible = None;
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| ParseError)?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| ParseError)?;
                    match attr.key.as_ref() {
                        b"type" if value.as_bytes() != node.name().as_ref() => {
                            return Err(ParseError);
                        }
                        b"name" => name = Some(value.into_owned()),
                        b"label" => label = Some(value.into_owned()),
                        b"bundleId" => bundle = Some(value == SPRINGBOARD),
                        b"visible" => {
                            visible = match value.as_ref() {
                                "true" => Some(true),
                                "false" => Some(false),
                                _ => None,
                            };
                        }
                        b"x" => x = value.parse::<f64>().ok(),
                        b"y" => y = value.parse::<f64>().ok(),
                        b"width" => width = value.parse::<f64>().ok(),
                        b"height" => height = value.parse::<f64>().ok(),
                        _ => {}
                    }
                }
                let kind = node.name();
                if matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeAlert"
                        | b"XCUIElementTypeSheet"
                        | b"XCUIElementTypeSecureTextField"
                        | b"XCUIElementTypeTextField"
                        | b"XCUIElementTypeKey"
                        | b"XCUIElementTypeIcon"
                ) || [name.as_deref(), label.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|value| {
                        matches!(
                            value,
                            "passcode-view"
                                | "Enter Passcode"
                                | "Trust"
                                | "Trust This Computer"
                                | "Trust This Computer?"
                        ) || (value.len() == 1 && value.as_bytes()[0].is_ascii_digit())
                    })
                {
                    return Err(ParseError);
                }
                let mut inside = stack.last().copied().unwrap_or(false);
                if kind.as_ref() == b"XCUIElementTypeApplication" {
                    apps += 1;
                    if apps != 1
                        || !(bundle == Some(true)
                            || (bundle.is_none()
                                && (name.as_deref() == Some("SpringBoard")
                                    || label.as_deref() == Some("SpringBoard"))))
                    {
                        return Err(ParseError);
                    }
                    inside = true;
                    if x != Some(0.0) || y != Some(0.0) {
                        return Err(ParseError);
                    }
                    size = width.zip(height).filter(|(w, h)| {
                        (200.0..=2000.0).contains(w) && (400.0..=2000.0).contains(h)
                    });
                }
                if name.as_deref() == Some("proud-lock") {
                    if !inside || kind.as_ref() != b"XCUIElementTypeOther" || visible.is_none() {
                        return Err(ParseError);
                    }
                    proud_visible = visible;
                    proud += 1;
                }
                if start {
                    stack.push(inside);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(ParseError)?;
            }
            Event::Text(text) if text.as_ref().iter().any(|b| !b.is_ascii_whitespace()) => {
                return Err(ParseError);
            }
            Event::DocType(_) | Event::CData(_) | Event::GeneralRef(_) => {
                return Err(ParseError);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || apps != 1 || proud != 1 || !stack.is_empty() {
        return Err(ParseError);
    }
    Ok(NormalLockScreen {
        size: size.ok_or(ParseError)?,
        proud_visible: proud_visible.ok_or(ParseError)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const NORMAL: &str = r#"<AppiumAUT><XCUIElementTypeApplication bundleId="com.apple.springboard" x="0" y="0" width="400" height="800"><XCUIElementTypeOther name="proud-lock" visible="true"/></XCUIElementTypeApplication></AppiumAUT>"#;

    #[test]
    fn normal_lock_requires_unambiguous_full_screen_springboard_without_controls() {
        assert_eq!(
            normal_lock_screen(NORMAL),
            Ok(NormalLockScreen {
                size: (400.0, 800.0),
                proud_visible: true
            })
        );
        assert!(
            normal_lock_screen(&NORMAL.replace(
                "name=\"proud-lock\"",
                "name=\"proud-lock\" type=\"XCUIElementTypeOther\""
            ))
            .is_ok()
        );
        for xml in [
            NORMAL.replace("x=\"0\"", ""),
            NORMAL.replace("y=\"0\"", "y=\"1\""),
            NORMAL.replace("x=\"0\"", "x=\"NaN\""),
            NORMAL.replace("width=\"400\"", "width=\"inf\""),
            NORMAL.replace("com.apple.springboard", "com.example.other"),
            NORMAL.replace("proud-lock", "other"),
            NORMAL.replace("visible=\"true\"", ""),
            format!("{NORMAL}<extra/>"),
            NORMAL.replace("</AppiumAUT>", ""),
        ] {
            assert_eq!(normal_lock_screen(&xml), Err(ParseError));
        }
        for extra in [
            "<XCUIElementTypeOther type=\"XCUIElementTypeSecureTextField\"/>",
            "<XCUIElementTypeOther type=\"XCUIElementTypeKey\"/>",
            "<XCUIElementTypeOther type=\"XCUIElementTypeAlert\"/>",
            "<XCUIElementTypeAlert/>",
            "<XCUIElementTypeSheet/>",
            "<XCUIElementTypeTextField/>",
            "<XCUIElementTypeSecureTextField/>",
            "<XCUIElementTypeKey/>",
            "<XCUIElementTypeIcon/>",
            "<XCUIElementTypeButton name=\"1\"/>",
            "<XCUIElementTypeStaticText label=\"Trust This Computer?\"/>",
            "<XCUIElementTypeOther name=\"passcode-view\"/>",
            "<XCUIElementTypeOther name=\"proud-lock\" visible=\"true\"/>",
            "<XCUIElementTypeApplication name=\"SpringBoard\"/>",
            "<a></b>",
            "<![CDATA[private]]>",
            "&amp;",
        ] {
            assert_eq!(
                normal_lock_screen(&NORMAL.replace(
                    "</XCUIElementTypeApplication>",
                    &format!("{extra}</XCUIElementTypeApplication>")
                )),
                Err(ParseError)
            );
        }
    }
}
