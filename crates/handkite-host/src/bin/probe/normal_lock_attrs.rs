use handkite_host::host_config;
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
    let summary = super::lock_ui_attrs::with_locked_xml(binding.usb_id, parse)?;
    println!("{}", summary.line());
    Ok(())
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

fn count_token(count: u8) -> &'static str {
    match count {
        0 => "zero",
        1 => "one",
        _ => "multiple",
    }
}

#[derive(Default)]
struct Summary {
    applications: u8,
    identity_match: bool,
    proud: u8,
    proud_other: bool,
    proud_under_app: bool,
    proud_visible: Flag,
    passcode_view: bool,
    key: bool,
    secure_field: bool,
    alert: bool,
    geometry_valid: bool,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            concat!(
                "normal_lock_attrs=observed application_count={} application_identity_match={} ",
                "proud_lock_count={} proud_lock_kind_other={} proud_lock_under_application={} proud_lock_visible={} ",
                "passcode_view_present={} key_present={} secure_field_present={} alert_present={} screen_geometry_valid={}",
            ),
            count_token(self.applications),
            if self.identity_match { "yes" } else { "no" },
            count_token(self.proud),
            self.proud_other,
            self.proud_under_app,
            self.proud_visible.token(),
            self.passcode_view,
            self.key,
            self.secure_field,
            self.alert,
            self.geometry_valid
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
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                if stack.len() >= 128 {
                    return Err(());
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots > 1 {
                        return Err(());
                    }
                }
                let (mut app_name, mut proud, mut bundle) = (false, false, None);
                let (mut width, mut height) = (None, None);
                let mut visible = Flag::Missing;
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| ())?;
                    let value = attr
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| ())?;
                    match attr.key.as_ref() {
                        b"name" | b"label" => {
                            app_name |= value == "SpringBoard";
                            proud |= attr.key.as_ref() == b"name" && value == "proud-lock";
                            summary.passcode_view |= value == "passcode-view";
                            summary.key |= value.len() == 1 && value.as_bytes()[0].is_ascii_digit();
                        }
                        b"bundleId" => bundle = Some(value == "com.apple.springboard"),
                        b"visible" => visible = Flag::parse(&value),
                        b"width" => width = value.parse::<f64>().ok(),
                        b"height" => height = value.parse::<f64>().ok(),
                        _ => {}
                    }
                }
                let kind = node.name();
                summary.key |= kind.as_ref() == b"XCUIElementTypeKey";
                summary.secure_field |= kind.as_ref() == b"XCUIElementTypeSecureTextField";
                summary.alert |= matches!(
                    kind.as_ref(),
                    b"XCUIElementTypeAlert" | b"XCUIElementTypeSheet"
                );
                let under_app = stack.last().copied().unwrap_or(false);
                let application = kind.as_ref() == b"XCUIElementTypeApplication";
                if application {
                    summary.applications = (summary.applications + 1).min(2);
                    summary.identity_match = summary.applications == 1
                        && (bundle == Some(true) || (bundle.is_none() && app_name));
                    summary.geometry_valid = summary.applications == 1
                        && width.zip(height).is_some_and(|(w, h)| {
                            (200.0..=2000.0).contains(&w) && (400.0..=2000.0).contains(&h)
                        });
                }
                if proud {
                    summary.proud = (summary.proud + 1).min(2);
                    summary.proud_other =
                        summary.proud == 1 && kind.as_ref() == b"XCUIElementTypeOther";
                    summary.proud_under_app = summary.proud == 1 && under_app;
                    summary.proud_visible = if summary.proud == 1 {
                        visible
                    } else {
                        Flag::Missing
                    };
                }
                if start {
                    stack.push(under_app || application);
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(())?;
            }
            Event::Text(text) if text.as_ref().iter().any(|b| !b.is_ascii_whitespace()) => {
                return Err(());
            }
            Event::DocType(_) | Event::CData(_) | Event::GeneralRef(_) => return Err(()),
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || !stack.is_empty() {
        return Err(());
    }
    if summary.applications != 1 {
        summary.proud_under_app = false;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROUD: &str = r#"<XCUIElementTypeOther name="proud-lock" visible="true"/>"#;
    const NORMAL: &str = "normal_lock_attrs=observed application_count=one application_identity_match=yes proud_lock_count=one proud_lock_kind_other=true proud_lock_under_application=true proud_lock_visible=true passcode_view_present=false key_present=false secure_field_present=false alert_present=false screen_geometry_valid=true";

    fn tree(content: &str) -> String {
        format!(
            r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" width="400" height="800">{content}</XCUIElementTypeApplication></AppiumAUT>"#
        )
    }

    #[test]
    fn output_is_exact_and_discards_private_attributes() {
        assert_eq!(parse(&tree(PROUD)).unwrap().line(), NORMAL);
        let xml = tree(PROUD).replace("visible=\"true\"", "visible=\"true\" label=\"fixture-private-label\" value=\"fixture-private-value\" x=\"123.456\" y=\"789.123\"");
        assert_eq!(parse(&xml).unwrap().line(), NORMAL);
        for (attribute, expected) in [
            ("visible=\"false\"", "false"),
            ("visible=\"fixture-private\"", "missing"),
            ("", "missing"),
        ] {
            assert_eq!(
                parse(&tree(&PROUD.replace("visible=\"true\"", attribute)))
                    .unwrap()
                    .line(),
                NORMAL.replace(
                    "proud_lock_visible=true",
                    &format!("proud_lock_visible={expected}")
                )
            );
        }
    }

    #[test]
    fn absent_duplicate_wrong_kind_and_wrong_ancestry_lose_evidence() {
        assert_eq!(parse(&tree("")).unwrap().line(), NORMAL.replace("proud_lock_count=one proud_lock_kind_other=true proud_lock_under_application=true proud_lock_visible=true", "proud_lock_count=zero proud_lock_kind_other=false proud_lock_under_application=false proud_lock_visible=missing"));
        assert_eq!(parse(&tree(&format!("{PROUD}{PROUD}"))).unwrap().line(), NORMAL.replace("proud_lock_count=one proud_lock_kind_other=true proud_lock_under_application=true proud_lock_visible=true", "proud_lock_count=multiple proud_lock_kind_other=false proud_lock_under_application=false proud_lock_visible=missing"));
        assert_eq!(
            parse(&tree(
                &PROUD.replace("XCUIElementTypeOther", "XCUIElementTypeButton")
            ))
            .unwrap()
            .line(),
            NORMAL.replace("proud_lock_kind_other=true", "proud_lock_kind_other=false")
        );
        let outside = tree("").replace("</AppiumAUT>", &format!("{PROUD}</AppiumAUT>"));
        assert_eq!(
            parse(&outside).unwrap().line(),
            NORMAL.replace(
                "proud_lock_under_application=true",
                "proud_lock_under_application=false"
            )
        );
        let label_only = parse(&tree(&PROUD.replace("name=", "label="))).unwrap();
        assert_eq!(label_only.proud, 0);
    }

    #[test]
    fn application_identity_count_and_dimensions_are_bounded() {
        let absent = parse(&format!("<AppiumAUT>{PROUD}</AppiumAUT>")).unwrap();
        assert_eq!(count_token(absent.applications), "zero");
        assert!(!absent.identity_match && !absent.geometry_valid && !absent.proud_under_app);
        for extra in [
            "<XCUIElementTypeApplication name=\"SpringBoard\" width=\"400\" height=\"800\"/>",
            "<XCUIElementTypeApplication/><XCUIElementTypeApplication/>",
        ] {
            let duplicate = parse(&tree(&format!("{PROUD}{extra}"))).unwrap();
            assert_eq!(count_token(duplicate.applications), "multiple");
            assert!(
                !duplicate.identity_match
                    && !duplicate.geometry_valid
                    && !duplicate.proud_under_app
            );
        }
        for identity in [
            "name=\"fixture-private-name\"",
            "name=\"SpringBoard\" bundleId=\"fixture-private-id\"",
        ] {
            assert_eq!(
                parse(&tree(PROUD).replace("name=\"SpringBoard\"", identity))
                    .unwrap()
                    .line(),
                NORMAL.replace(
                    "application_identity_match=yes",
                    "application_identity_match=no"
                )
            );
        }
        assert_eq!(
            parse(
                &tree(PROUD).replace("name=\"SpringBoard\"", "bundleId=\"com.apple.springboard\"")
            )
            .unwrap()
            .line(),
            NORMAL
        );
        for dimension in ["0", "NaN", "inf", "2001", "invalid"] {
            assert_eq!(
                parse(&tree(PROUD).replace("width=\"400\"", &format!("width=\"{dimension}\"")))
                    .unwrap()
                    .line(),
                NORMAL.replace("screen_geometry_valid=true", "screen_geometry_valid=false")
            );
        }
    }

    #[test]
    fn prohibited_presence_is_reported_without_enabling_input() {
        for (extra, field) in [
            (
                "<XCUIElementTypeOther name=\"passcode-view\"/>",
                "passcode_view_present",
            ),
            (
                "<XCUIElementTypeOther label=\"passcode-view\"/>",
                "passcode_view_present",
            ),
            ("<XCUIElementTypeKey/>", "key_present"),
            ("<XCUIElementTypeButton name=\"1\"/>", "key_present"),
            (
                "<XCUIElementTypeSecureTextField value=\"fixture-private-value\"/>",
                "secure_field_present",
            ),
            ("<XCUIElementTypeAlert/>", "alert_present"),
            ("<XCUIElementTypeSheet/>", "alert_present"),
        ] {
            assert_eq!(
                parse(&tree(&format!("{PROUD}{extra}"))).unwrap().line(),
                NORMAL.replace(&format!("{field}=false"), &format!("{field}=true"))
            );
        }
    }

    #[test]
    fn malformed_or_unbounded_xml_and_unsafe_arguments_refuse_statically() {
        for xml in [
            String::new(),
            "<a>".into(),
            "<a></b>".into(),
            format!("{}<extra/>", tree(PROUD)),
            tree("<a name=\"x\" name=\"y\"/>"),
            tree("private text"),
            tree("&amp;"),
            tree("<![CDATA[private]]>"),
            format!("<!DOCTYPE private>{}", tree(PROUD)),
            tree(&format!("{}{}", "<a>".repeat(130), "</a>".repeat(130))),
            " ".repeat(2 * 1024 * 1024 + 1),
        ] {
            assert!(parse(&xml).is_err());
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
