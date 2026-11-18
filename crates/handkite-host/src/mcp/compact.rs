//! Optional presentation only; the full capture remains the admission evidence.
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

const PREFIX: &str = "XCUIElementType";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_VALUE: usize = 64 * 1024;
const ATTRIBUTES: &[&str] = &[
    "type",
    "source",
    "input_source",
    "target_id",
    "name",
    "label",
    "value",
    "enabled",
    "visible",
    "selected",
    "focused",
    "accessible",
    "hittable",
    "index",
    "x",
    "y",
    "width",
    "height",
    "bundleId",
    "pid",
    "processId",
    "placeholderValue",
    "nativeFrame",
    "nativeAccessibilityElement",
    "traits",
    "minValue",
    "maxValue",
    "customActions",
];
const ROLES: &[&str] = &[
    "Any",
    "Other",
    "Application",
    "Group",
    "Window",
    "Sheet",
    "Drawer",
    "Alert",
    "Dialog",
    "Button",
    "RadioButton",
    "RadioGroup",
    "CheckBox",
    "DisclosureTriangle",
    "PopUpButton",
    "ComboBox",
    "MenuButton",
    "ToolbarButton",
    "Popover",
    "Keyboard",
    "Key",
    "NavigationBar",
    "TabBar",
    "TabGroup",
    "Toolbar",
    "StatusBar",
    "Table",
    "TableRow",
    "TableColumn",
    "Outline",
    "OutlineRow",
    "Browser",
    "CollectionView",
    "Slider",
    "PageIndicator",
    "ProgressIndicator",
    "ActivityIndicator",
    "SegmentedControl",
    "Picker",
    "PickerWheel",
    "Switch",
    "Toggle",
    "Link",
    "Image",
    "Icon",
    "SearchField",
    "ScrollView",
    "ScrollBar",
    "StaticText",
    "TextField",
    "SecureTextField",
    "DatePicker",
    "TextView",
    "Menu",
    "MenuItem",
    "MenuBar",
    "MenuBarItem",
    "Map",
    "WebView",
    "IncrementArrow",
    "DecrementArrow",
    "Timeline",
    "RatingIndicator",
    "ValueIndicator",
    "SplitGroup",
    "Splitter",
    "RelevanceIndicator",
    "ColorWell",
    "HelpTag",
    "Matte",
    "DockItem",
    "Ruler",
    "RulerMarker",
    "Grid",
    "LevelIndicator",
    "Cell",
    "LayoutArea",
    "LayoutItem",
    "Handle",
    "Stepper",
    "Tab",
    "TouchBar",
    "StatusItem",
];

#[derive(Debug, PartialEq)]
pub(super) enum Fallback {
    Malformed,
    Unsupported,
    Limit,
}
impl Fallback {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed_xml",
            Self::Unsupported => "unsupported_xml",
            Self::Limit => "limit_exceeded",
        }
    }
}

fn validate_text(value: &str) -> Result<(), Fallback> {
    if value.len() > MAX_VALUE {
        return Err(Fallback::Limit);
    }
    if value.chars().any(
        |c| !matches!(c as u32, 9 | 10 | 13 | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff),
    ) {
        return Err(Fallback::Malformed);
    }
    Ok(())
}

fn role(tag: &[u8]) -> Result<&str, Fallback> {
    let tag = std::str::from_utf8(tag).map_err(|_| Fallback::Malformed)?;
    if tag == "AppiumAUT" {
        return Ok(tag);
    }
    tag.strip_prefix(PREFIX)
        .filter(|role| ROLES.contains(role))
        .ok_or(Fallback::Unsupported)
}

fn attribute_separator(event: &BytesStart<'_>, key: &[u8]) -> Result<(), Fallback> {
    let offset = (key.as_ptr() as usize)
        .checked_sub(event.as_ref().as_ptr() as usize)
        .ok_or(Fallback::Malformed)?;
    if offset == 0
        || !matches!(
            event.as_ref().get(offset - 1),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        )
    {
        return Err(Fallback::Malformed);
    }
    Ok(())
}

// Return the source range of the raw type value, preserving all other bytes.
// quick-xml borrows attribute slices from BytesStart; range checks fail closed.
fn element(
    reader: &Reader<&[u8]>,
    event: &BytesStart<'_>,
) -> Result<Option<(usize, usize, String)>, Fallback> {
    let name = event.name();
    let alias = role(name.as_ref())?;
    let mut replacement = None;
    for attr in event.attributes() {
        let attr = attr.map_err(|_| Fallback::Malformed)?;
        attribute_separator(event, attr.key.as_ref())?;
        let key = std::str::from_utf8(attr.key.as_ref()).map_err(|_| Fallback::Malformed)?;
        if !ATTRIBUTES.contains(&key) {
            return Err(Fallback::Unsupported);
        }
        if attr.value.contains(&b'<') {
            return Err(Fallback::Malformed);
        }
        let value = attr
            .decode_and_unescape_value(reader.decoder())
            .map_err(|_| Fallback::Malformed)?;
        validate_text(&value)?;
        if key == "type" {
            if value.as_bytes() != name.as_ref() {
                return Err(Fallback::Unsupported);
            }
            if alias != "AppiumAUT" {
                let start = (attr.value.as_ptr() as usize)
                    .checked_sub(event.as_ref().as_ptr() as usize)
                    .ok_or(Fallback::Unsupported)?;
                let end = start
                    .checked_add(attr.value.len())
                    .ok_or(Fallback::Unsupported)?;
                if event.as_ref().get(start..end) != Some(attr.value.as_ref()) {
                    return Err(Fallback::Unsupported);
                }
                replacement = Some((start, end, alias.to_owned()));
            }
        }
    }
    Ok(replacement)
}

pub(super) fn project(xml: &str) -> Result<String, Fallback> {
    if xml.len() > MAX_BYTES {
        return Err(Fallback::Limit);
    }
    let input = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let offset = xml.len() - input.len();
    let mut reader = Reader::from_str(input);
    reader.config_mut().check_end_names = true;
    let mut output = String::with_capacity(xml.len());
    output.push_str(&xml[..offset]);
    let mut stack: Vec<usize> = Vec::new();
    let (mut roots, mut nodes, mut declared) = (0, 0, false);
    loop {
        let start = offset + reader.buffer_position() as usize;
        let event = reader.read_event().map_err(|_| Fallback::Malformed)?;
        let end = offset + reader.buffer_position() as usize;
        let chunk = xml.get(start..end).ok_or(Fallback::Malformed)?;
        let is_start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(event) | Event::Empty(event) => {
                nodes += 1;
                if nodes > MAX_NODES || stack.len() >= MAX_DEPTH {
                    return Err(Fallback::Limit);
                }
                if stack.is_empty() {
                    roots += 1;
                    if roots != 1 {
                        return Err(Fallback::Malformed);
                    }
                } else {
                    *stack.last_mut().unwrap() = 0;
                }
                let name = event.name();
                let alias = role(name.as_ref())?;
                let replacement = element(&reader, &event)?;
                if chunk.as_bytes().get(1..1 + event.len()) != Some(event.as_ref()) {
                    return Err(Fallback::Unsupported);
                }
                output.push('<');
                output.push_str(alias);
                let mut cursor = 1 + name.as_ref().len();
                if let Some((from, to, alias)) = replacement {
                    output.push_str(&chunk[cursor..1 + from]);
                    output.push_str(&alias);
                    cursor = 1 + to;
                }
                output.push_str(&chunk[cursor..]);
                if is_start {
                    stack.push(0);
                }
            }
            Event::End(event) => {
                stack.pop().ok_or(Fallback::Malformed)?;
                let name = event.name();
                let alias = role(name.as_ref())?;
                if chunk.as_bytes().get(2..2 + name.as_ref().len()) != Some(name.as_ref()) {
                    return Err(Fallback::Unsupported);
                }
                output.push_str("</");
                output.push_str(alias);
                output.push_str(&chunk[2 + name.as_ref().len()..]);
            }
            Event::Text(event) => {
                if event.as_ref().windows(3).any(|value| value == b"]]>") {
                    return Err(Fallback::Malformed);
                }
                let text = event.decode().map_err(|_| Fallback::Malformed)?;
                validate_text(&text)?;
                if let Some(length) = stack.last_mut() {
                    *length += text.len();
                    if *length > MAX_VALUE {
                        return Err(Fallback::Limit);
                    }
                } else if !text
                    .bytes()
                    .all(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    return Err(Fallback::Malformed);
                }
                output.push_str(chunk);
            }
            Event::GeneralRef(event) => {
                let length = stack.last_mut().ok_or(Fallback::Malformed)?;
                let entity = format!("&{};", event.decode().map_err(|_| Fallback::Malformed)?);
                let value =
                    quick_xml::escape::unescape(&entity).map_err(|_| Fallback::Malformed)?;
                validate_text(&value)?;
                *length += value.len();
                if *length > MAX_VALUE {
                    return Err(Fallback::Limit);
                }
                output.push_str(chunk);
            }
            Event::Decl(decl) if nodes == 0 && !declared => {
                if start != 0 && !(start == 3 && xml.starts_with('\u{feff}')) {
                    return Err(Fallback::Malformed);
                }
                if decl.version().map_err(|_| Fallback::Malformed)?.as_ref() != b"1.0"
                    || decl
                        .encoding()
                        .transpose()
                        .map_err(|_| Fallback::Malformed)?
                        .is_some_and(|value| !value.eq_ignore_ascii_case(b"utf-8"))
                {
                    return Err(Fallback::Unsupported);
                }
                let content = std::str::from_utf8(&decl).map_err(|_| Fallback::Malformed)?;
                let declaration = BytesStart::from_content(content, 3);
                let mut last = 0;
                for attr in declaration.attributes() {
                    let attr = attr.map_err(|_| Fallback::Malformed)?;
                    attribute_separator(&declaration, attr.key.as_ref())?;
                    let position = match attr.key.as_ref() {
                        b"version" => 1,
                        b"encoding" => 2,
                        b"standalone" if matches!(attr.value.as_ref(), b"yes" | b"no") => 3,
                        _ => return Err(Fallback::Unsupported),
                    };
                    if position <= last {
                        return Err(Fallback::Malformed);
                    }
                    last = position;
                }
                declared = true;
                output.push_str(chunk);
            }
            Event::Eof => break,
            _ => return Err(Fallback::Unsupported),
        }
        if output.len() > MAX_BYTES {
            return Err(Fallback::Limit);
        }
    }
    if roots != 1 || !stack.is_empty() {
        return Err(Fallback::Malformed);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_roles_and_matching_type_values_change() {
        let xml = "<?xml version='1.0'?>\n<AppiumAUT>\n <XCUIElementTypeApplication type = 'XCUIElementTypeApplication' name='Café &amp; 🦊'><XCUIElementTypeButton label = \"A &quot;✓&quot;\" value='&#x3b1;' enabled='true' selected='false' x='01.0' y='-2' width='30' height='40'/> α &lt; β &#33;\n<XCUIElementTypeOther type='XCUIElementTypeOth&#101;r'></XCUIElementTypeOther> </XCUIElementTypeApplication>\n</AppiumAUT> ";
        let expected = "<?xml version='1.0'?>\n<AppiumAUT>\n <Application type = 'Application' name='Café &amp; 🦊'><Button label = \"A &quot;✓&quot;\" value='&#x3b1;' enabled='true' selected='false' x='01.0' y='-2' width='30' height='40'/> α &lt; β &#33;\n<Other type='Other'></Other> </Application>\n</AppiumAUT> ";
        assert_eq!(project(xml).unwrap(), expected);
    }

    #[test]
    fn application_process_id_is_preserved() {
        let xml = "<?xml version='1.0'?><AppiumAUT><XCUIElementTypeApplication type='XCUIElementTypeApplication' processId='0042'/></AppiumAUT>";
        assert_eq!(
            project(xml).unwrap(),
            "<?xml version='1.0'?><AppiumAUT><Application type='Application' processId='0042'/></AppiumAUT>"
        );
    }

    #[test]
    fn malformed_and_unsupported_xml_never_partially_project() {
        for xml in [
            "",
            "<XCUIElementTypeButton>",
            "<XCUIElementTypeButton></other>",
            "<XCUIElementTypeButton label='a' label='b'/>",
            "<XCUIElementTypeButton label='<raw'/>",
            "<AppiumAUT>text]]>text</AppiumAUT>",
            "&#32;<AppiumAUT/>",
            "<?xml version='1.0' version='1.0'?><AppiumAUT/>",
            " <?xml version='1.0'?><AppiumAUT/>",
            "\n<?xml version='1.0'?><AppiumAUT/>",
            "<?xml version='1.0'encoding='UTF-8'?><AppiumAUT/>",
            "<XCUIElementTypeButton label='a'enabled='true'/>",
            "<XCUIElementTypeButton>&unknown;</XCUIElementTypeButton>",
            "<XCUIElementTypeButton>&#0;</XCUIElementTypeButton>",
            "<XCUIElementTypeButton/><XCUIElementTypeButton/>",
            "text<XCUIElementTypeButton/>",
        ] {
            assert_eq!(project(xml), Err(Fallback::Malformed), "{xml}");
        }
        for xml in [
            "<unknown/>",
            "<XCUIElementTypeFutureRole/>",
            "<XCUIElementTypeButton uuid='action-id'/>",
            "<XCUIElementTypeButton type='XCUIElementTypeOther'/>",
            "<!DOCTYPE AppiumAUT><AppiumAUT/>",
            "<AppiumAUT><!--hidden--></AppiumAUT>",
            "<AppiumAUT><![CDATA[text]]></AppiumAUT>",
            "<?custom value?><AppiumAUT/>",
            "<?xml version='1.0' encoding='UTF-16'?><AppiumAUT/>",
            "<?xml version='1.0' unknown='value'?><AppiumAUT/>",
            "<?xml version='1.0' standalone='maybe'?><AppiumAUT/>",
        ] {
            assert_eq!(project(xml), Err(Fallback::Unsupported), "{xml}");
        }
    }

    #[test]
    fn whitespace_without_declaration_and_separated_attributes_remain_valid() {
        assert_eq!(project(" \n<AppiumAUT/>"), Ok(" \n<AppiumAUT/>".into()));
        let xml = "<?xml\tversion='1.0'\nencoding='UTF-8'\rstandalone='yes'?><AppiumAUT/>";
        assert_eq!(project(xml), Ok(xml.into()));
        let xml = "\u{feff}<?xml version='1.0'?><AppiumAUT/>";
        assert_eq!(project(xml), Ok(xml.into()));
        let xml = "\u{feff}<XCUIElementTypeButton/>";
        assert_eq!(project(xml), Ok("\u{feff}<Button/>".into()));
    }

    #[test]
    fn bounds_input_nodes_depth_and_values() {
        assert_eq!(project(&" ".repeat(MAX_BYTES + 1)), Err(Fallback::Limit));
        let nodes = |count| {
            format!(
                "<AppiumAUT>{}</AppiumAUT>",
                "<XCUIElementTypeOther/>".repeat(count)
            )
        };
        assert!(project(&nodes(MAX_NODES - 1)).is_ok());
        assert_eq!(project(&nodes(MAX_NODES)), Err(Fallback::Limit));
        let deep = |count| {
            format!(
                "{}{}",
                "<XCUIElementTypeOther>".repeat(count),
                "</XCUIElementTypeOther>".repeat(count)
            )
        };
        assert!(project(&deep(MAX_DEPTH)).is_ok());
        assert_eq!(project(&deep(MAX_DEPTH + 1)), Err(Fallback::Limit));
        assert_eq!(
            project(&format!(
                "<AppiumAUT label='{}'/>",
                "x".repeat(MAX_VALUE + 1)
            )),
            Err(Fallback::Limit)
        );
        assert_eq!(
            project(&format!(
                "<AppiumAUT>{}</AppiumAUT>",
                "x".repeat(MAX_VALUE + 1)
            )),
            Err(Fallback::Limit)
        );
        assert_eq!(
            project(&format!(
                "<AppiumAUT>{}&#32;x</AppiumAUT>",
                "x".repeat(MAX_VALUE)
            )),
            Err(Fallback::Limit)
        );
    }
}
