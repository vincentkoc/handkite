use super::*;
use crate::wda_ui_admission::Attributes;
use quick_xml::events::Event;

pub(super) const BODY: &str = "Notifications may include alerts, sounds and icon badges. These can be configured in Settings.";
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Node {
    pub kind: String,
    pub attrs: Attributes,
    pub parent: Option<usize>,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Tree {
    pub nodes: Vec<Node>,
    pub app: usize,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Modal {
    pub tree: Tree,
    pub alert: usize,
    pub buttons: Vec<usize>,
    pub texts: Vec<usize>,
    pub title: String,
    pub body: String,
    pub fingerprint: Vec<Node>,
    pub viewport: [f64; 4],
}
pub(super) fn rect(a: &Attributes) -> Result<[f64; 4], AdapterError> {
    let mut r = [0.; 4];
    for (i, k) in ["x", "y", "width", "height"].iter().enumerate() {
        r[i] = a
            .get(*k)
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .ok_or(AdapterError)?;
    }
    if r[0] < 0. || r[1] < 0. || r[2] <= 0. || r[3] <= 0. {
        return Err(AdapterError);
    }
    Ok(r)
}
fn encloses(a: [f64; 4], b: [f64; 4]) -> bool {
    b[0] >= a[0] && b[1] >= a[1] && b[0] + b[2] <= a[0] + a[2] && b[1] + b[3] <= a[1] + a[3]
}
fn visible(a: &Attributes) -> bool {
    a.get("visible").map(String::as_str) == Some("true")
        && a.get("enabled").map(String::as_str) == Some("true")
}
fn forbidden(kind: &str, a: &Attributes) -> bool {
    matches!(
        kind,
        "XCUIElementTypeSecureTextField"
            | "XCUIElementTypeTextField"
            | "XCUIElementTypeTextView"
            | "XCUIElementTypeSearchField"
            | "XCUIElementTypeKeyboard"
            | "XCUIElementTypeSheet"
    ) || a.values().any(|s| {
        let s = s
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .collect::<String>()
            .to_lowercase();
        [
            "passcode",
            "password",
            "proud-lock",
            "call-to-action-label",
            "verification code",
            "sign in",
            "sign-in",
            "apple id",
            "face id",
            "touch id",
            "trust this",
            "ui automation",
            "xctest",
            "purchase",
            "payment",
            "terms",
        ]
        .iter()
        .any(|v| s.contains(&v.chars().filter(|c| !c.is_whitespace()).collect::<String>()))
    })
}
pub(super) fn scan(xml: &str) -> Result<Tree, AdapterError> {
    if xml.len() > 2 * 1024 * 1024 {
        return Err(AdapterError);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut roots = 0;
    loop {
        match reader.read_event().map_err(|_| AdapterError)? {
            e @ (Event::Start(_) | Event::Empty(_)) => {
                let start = matches!(&e, Event::Start(_));
                let n = match e {
                    Event::Start(n) | Event::Empty(n) => n,
                    _ => unreachable!(),
                };
                let kind = std::str::from_utf8(n.name().as_ref())
                    .map_err(|_| AdapterError)?
                    .to_owned();
                let mut attrs = Attributes::new();
                for a in n.attributes() {
                    let a = a.map_err(|_| AdapterError)?;
                    let k = std::str::from_utf8(a.key.as_ref())
                        .map_err(|_| AdapterError)?
                        .to_owned();
                    let v = a
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| AdapterError)?
                        .into_owned();
                    if v.chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                        || attrs.insert(k, v).is_some()
                    {
                        return Err(AdapterError);
                    }
                }
                if attrs.get("type").is_some_and(|t| t != &kind) || forbidden(&kind, &attrs) {
                    return Err(AdapterError);
                }
                if stack.is_empty() {
                    roots += 1;
                }
                let i = nodes.len();
                nodes.push(Node {
                    kind,
                    attrs,
                    parent: stack.last().copied(),
                });
                if nodes.len() > 20000 {
                    return Err(AdapterError);
                }
                if start {
                    stack.push(i);
                }
            }
            Event::End(e) => {
                let i = stack.pop().ok_or(AdapterError)?;
                if nodes[i].kind.as_bytes() != e.name().as_ref() {
                    return Err(AdapterError);
                }
            }
            Event::Text(t) if t.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Event::Decl(_) if nodes.is_empty() => {}
            Event::Eof => {
                if !stack.is_empty() || roots != 1 {
                    return Err(AdapterError);
                }
                break;
            }
            _ => return Err(AdapterError),
        }
    }
    let apps = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind == "XCUIElementTypeApplication")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if apps.len() != 1 {
        return Err(AdapterError);
    }
    Ok(Tree {
        nodes,
        app: apps[0],
    })
}
pub(super) fn owner(tree: &Tree) -> Result<(String, u64), AdapterError> {
    let a = &tree.nodes[tree.app].attrs;
    let bundle = a
        .get("bundleId")
        .filter(|s| !s.is_empty())
        .ok_or(AdapterError)?
        .clone();
    let pid = a
        .get("processId")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .ok_or(AdapterError)?;
    Ok((bundle, pid))
}
fn within(t: &Tree, mut i: usize, parent: usize) -> bool {
    loop {
        if i == parent {
            return true;
        }
        match t.nodes[i].parent {
            Some(p) => i = p,
            None => return false,
        }
    }
}
fn text(a: &Attributes) -> Result<String, AdapterError> {
    let s = a
        .get("label")
        .filter(|s| !s.is_empty())
        .ok_or(AdapterError)?;
    if s.len() > 2048 {
        return Err(AdapterError);
    }
    Ok(s.split_whitespace().collect::<Vec<_>>().join(" "))
}
pub(super) fn modal(tree: Tree, expected_pid: u64) -> Result<Modal, AdapterError> {
    modal_with_visibility(tree, expected_pid, true)
}
pub(super) fn semantic_modal(tree: Tree, expected_pid: u64) -> Result<Modal, AdapterError> {
    modal_with_visibility(tree, expected_pid, false)
}
fn modal_with_visibility(
    tree: Tree,
    expected_pid: u64,
    require_visible: bool,
) -> Result<Modal, AdapterError> {
    let admitted = |a: &Attributes| {
        if require_visible {
            visible(a)
        } else {
            a.get("enabled").map(String::as_str) == Some("true")
                && a.get("visible").is_none_or(|v| v == "true")
        }
    };
    if owner(&tree)? != ("com.apple.springboard".into(), expected_pid) {
        return Err(AdapterError);
    }
    let indexes = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind == "XCUIElementTypeAlert")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if indexes.len() != 1 {
        return Err(AdapterError);
    }
    let alert = indexes[0];
    let a = &tree.nodes[alert];
    if !within(&tree, alert, tree.app) || !admitted(&a.attrs) {
        return Err(AdapterError);
    }
    let viewport = rect(&tree.nodes[tree.app].attrs)?;
    let area = rect(&a.attrs)?;
    if viewport[0] != 0. || viewport[1] != 0. || !encloses(viewport, area) {
        return Err(AdapterError);
    }
    let mut buttons = Vec::new();
    let mut texts = Vec::new();
    let mut text_indexes = Vec::new();
    let mut fingerprint = Vec::new();
    for (i, n) in tree.nodes.iter().enumerate() {
        if !within(&tree, i, alert) {
            continue;
        }
        fingerprint.push(n.clone());
        if !matches!(
            n.kind.as_str(),
            "XCUIElementTypeAlert"
                | "XCUIElementTypeOther"
                | "XCUIElementTypeScrollView"
                | "XCUIElementTypeStaticText"
                | "XCUIElementTypeButton"
        ) {
            return Err(AdapterError);
        }
        if n.kind == "XCUIElementTypeButton" {
            if !admitted(&n.attrs) || !encloses(area, rect(&n.attrs)?) {
                return Err(AdapterError);
            }
            buttons.push(i);
        }
        if n.kind == "XCUIElementTypeStaticText" {
            if (require_visible && n.attrs.get("visible").map(String::as_str) != Some("true"))
                || n.attrs.get("visible").is_some_and(|v| v != "true")
            {
                return Err(AdapterError);
            }
            texts.push(text(&n.attrs)?);
            text_indexes.push(i);
        }
    }
    if texts.len() != 2 || buttons.len() != 2 {
        return Err(AdapterError);
    }
    let suffix = " Would Like to Send You Notifications";
    let title = texts
        .iter()
        .find(|t| t.ends_with(suffix))
        .ok_or(AdapterError)?
        .clone();
    let prefix = title.strip_suffix(suffix).ok_or(AdapterError)?;
    if prefix.len() < 3
        || !((prefix.starts_with('“') && prefix.ends_with('”'))
            || (prefix.starts_with('"') && prefix.ends_with('"')))
    {
        return Err(AdapterError);
    }
    let body = texts
        .iter()
        .find(|t| t.as_str() == BODY)
        .ok_or(AdapterError)?
        .clone();
    let labels = buttons
        .iter()
        .map(|i| text(&tree.nodes[*i].attrs))
        .collect::<Result<Vec<_>, _>>()?;
    if labels.iter().filter(|s| s.as_str() == "Allow").count() != 1
        || labels
            .iter()
            .filter(|s| matches!(s.as_str(), "Don't Allow" | "Don’t Allow"))
            .count()
            != 1
    {
        return Err(AdapterError);
    }
    // Parent indices outside the modal must not make background changes significant.
    for n in &mut fingerprint {
        n.parent = n.parent.map(|p| p.saturating_sub(alert));
    }
    Ok(Modal {
        tree,
        alert,
        buttons,
        texts: text_indexes,
        title,
        body,
        fingerprint,
        viewport,
    })
}
pub(super) fn matches(row: &Value, node: &Node) -> bool {
    row["enabled"].as_bool() == Some(true) && matches_fields(row, node)
}
pub(super) fn matches_text(row: &Value, node: &Node) -> bool {
    let enabled = match node.attrs.get("enabled").map(String::as_str) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    enabled.is_some() && row["enabled"].as_bool() == enabled && matches_fields(row, node)
}
fn matches_fields(row: &Value, node: &Node) -> bool {
    if row["type"].as_str() != Some(node.kind.as_str()) || row["displayed"].as_bool() != Some(true)
    {
        return false;
    }
    for k in ["label", "name", "value"] {
        let key = if k == "label" {
            "label".to_owned()
        } else {
            format!("attribute/{k}")
        };
        match row.get(&key) {
            Some(Value::String(v)) if node.attrs.get(k).map(String::as_str).unwrap_or("") == v => {}
            Some(Value::Null) if node.attrs.get(k).is_none_or(String::is_empty) => {}
            _ => return false,
        }
    }
    rect(&node.attrs).ok().is_some_and(|r| {
        ["x", "y", "width", "height"]
            .iter()
            .enumerate()
            .all(|(i, k)| row["rect"][*k].as_f64() == Some(r[i]))
    })
}
#[cfg(test)]
pub(super) fn fixture() -> String {
    format!(
        r#"<AppiumAUT><XCUIElementTypeApplication bundleId="com.apple.springboard" processId="42" x="0" y="0" width="100" height="200"><XCUIElementTypeWindow><XCUIElementTypeAlert label="notification" name="notification" value="" visible="true" enabled="true" x="10" y="40" width="80" height="120"><XCUIElementTypeStaticText label="“Fixture” Would Like to Send You Notifications" name="“Fixture” Would Like to Send You Notifications" value="" enabled="true" visible="true" x="15" y="45" width="70" height="25"/><XCUIElementTypeStaticText label="{BODY}" name="{BODY}" value="" enabled="true" visible="true" x="15" y="75" width="70" height="50"/><XCUIElementTypeButton label="Don't Allow" name="Don't Allow" value="" visible="true" enabled="true" x="15" y="140" width="30" height="15"/><XCUIElementTypeButton label="Allow" name="Allow" value="" visible="true" enabled="true" x="55" y="140" width="30" height="15"/></XCUIElementTypeAlert></XCUIElementTypeWindow></XCUIElementTypeApplication></AppiumAUT>"#
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_text_visibility_and_enabled_state_must_match_exactly() {
        let tree = scan(&fixture()).unwrap();
        let mut node = tree
            .nodes
            .iter()
            .find(|n| n.kind == "XCUIElementTypeStaticText")
            .unwrap()
            .clone();
        let mut row = json!({"type":node.kind,"enabled":true,"displayed":true,
            "label":node.attrs["label"],"attribute/name":node.attrs["name"],"attribute/value":"",
            "rect":{"x":15,"y":45,"width":70,"height":25}});
        assert!(matches_text(&row, &node));
        node.attrs.insert("enabled".into(), "false".into());
        assert!(!matches_text(&row, &node));
        row["enabled"] = json!(false);
        assert!(matches_text(&row, &node));
        node.attrs.insert("enabled".into(), "true".into());
        assert!(!matches_text(&row, &node));
        row["enabled"] = json!(true);
        row["displayed"] = json!(false);
        assert!(!matches_text(&row, &node));
        row["displayed"] = json!(true);
        for bad in [Value::Null, json!("true"), json!(1)] {
            row["enabled"] = bad;
            assert!(!matches_text(&row, &node));
        }
        row["enabled"] = json!(true);
        node.attrs.remove("enabled");
        assert!(!matches_text(&row, &node));
        node.attrs.insert("enabled".into(), "unknown".into());
        assert!(!matches_text(&row, &node));
    }
    #[test]
    fn cheap_context_is_structural_only_and_does_not_relax_legacy_visibility() {
        let cheap = fixture().replace(" visible=\"true\"", "");
        assert!(modal(scan(&cheap).unwrap(), 42).is_err());
        assert!(semantic_modal(scan(&cheap).unwrap(), 42).is_ok());
        let hidden = cheap.replace(
            "<XCUIElementTypeButton",
            "<XCUIElementTypeButton visible=\"false\"",
        );
        assert!(semantic_modal(scan(&hidden).unwrap(), 42).is_err());
        let auth = cheap.replace(
            "</XCUIElementTypeApplication>",
            "<XCUIElementTypeSecureTextField/></XCUIElementTypeApplication>",
        );
        assert!(scan(&auth).is_err());
    }
    #[test]
    fn notification_scroll_containers_preserve_structure_without_adding_targets() {
        let wrapped = fixture()
            .replace(
                "<XCUIElementTypeStaticText",
                "<XCUIElementTypeScrollView><XCUIElementTypeStaticText",
            )
            .replace(
                "height=\"25\"/>",
                "height=\"25\"/></XCUIElementTypeScrollView>",
            )
            .replace(
                "height=\"50\"/>",
                "height=\"50\"/></XCUIElementTypeScrollView>",
            );
        let modal = modal(scan(&wrapped).unwrap(), 42).unwrap();
        assert_eq!(modal.buttons.len(), 2);
        assert_eq!(
            modal
                .fingerprint
                .iter()
                .filter(|n| n.kind == "XCUIElementTypeScrollView")
                .count(),
            2
        );
        assert_ne!(
            modal.fingerprint,
            super::modal(scan(&fixture()).unwrap(), 42)
                .unwrap()
                .fingerprint
        );
        for kind in [
            "XCUIElementTypeMap",
            "XCUIElementTypeSwitch",
            "XCUIElementTypeSlider",
            "XCUIElementTypeUnknown",
        ] {
            let xml = wrapped.replace("XCUIElementTypeScrollView", kind);
            assert!(
                scan(&xml).and_then(|t| super::modal(t, 42)).is_err(),
                "{kind}"
            );
        }
        let auth = wrapped.replace(
            "<XCUIElementTypeScrollView>",
            "<XCUIElementTypeScrollView><XCUIElementTypeSecureTextField visible='false'/>",
        );
        assert!(scan(&auth).is_err());
    }
    #[test]
    fn complete_notification_requires_native_owner_and_exact_supported_content() {
        assert!(modal(scan(&fixture()).unwrap(), 42).is_ok());
        for s in [
            fixture().replace("com.apple.springboard", "com.example.lookalike"),
            fixture().replace("processId=\"42\"", "processId=\"0\""),
            fixture().replace("processId=\"42\"", "processId=\"43\""),
            fixture().replace(BODY, "Enter password to continue"),
            fixture().replace("label=\"Allow\"", "label=\"Continue\""),
            fixture().replace("width=\"30\"", "width=\"90\""),
            fixture().replace("enabled=\"true\"", "enabled=\"false\""),
            fixture().replace("visible=\"true\"", "visible=\"false\""),
        ] {
            assert!(scan(&s).and_then(|t| modal(t, 42)).is_err());
        }
    }
    #[test]
    fn hidden_auth_fields_second_alert_and_unknown_children_refuse() {
        for extra in [
            "<XCUIElementTypeSecureTextField visible='false'/>",
            "<XCUIElementTypeKeyboard visible='false'/>",
            "<XCUIElementTypeAlert visible='false'/>",
            "<XCUIElementTypeSheet/>",
            "<XCUIElementTypeOther label='XCTest Enable UI Automation'/>",
        ] {
            let s = fixture().replace(
                "</XCUIElementTypeApplication>",
                &format!("{extra}</XCUIElementTypeApplication>"),
            );
            assert!(scan(&s).and_then(|t| modal(t, 42)).is_err());
        }
        assert!(
            modal(
                scan(&fixture().replace("<XCUIElementTypeStaticText", "<XCUIElementTypeMap"))
                    .unwrap(),
                42
            )
            .is_err()
        );
    }
    #[test]
    fn contradictory_types_duplicate_attributes_and_xml_entities_refuse() {
        for s in [
            fixture().replace("bundleId=", "type='Other' bundleId="),
            fixture().replace("processId=\"42\"", "processId=\"42\" processId=\"42\""),
            format!("<!DOCTYPE x [<!ENTITY a 'x'>]>{}", fixture()),
        ] {
            assert!(scan(&s).is_err());
        }
    }
    #[test]
    fn hidden_control_split_auth_markers_are_rejected() {
        for marker in [
            "pass&#xA;code-view",
            "proud-&#x9;lock",
            "sign&#xA;in",
            "verification&#x9;code",
        ] {
            let xml=fixture().replace("</XCUIElementTypeApplication>",&format!("<XCUIElementTypeOther name='{marker}' visible='false'/></XCUIElementTypeApplication>"));
            assert!(scan(&xml).is_err(), "{marker}");
        }
    }
}
