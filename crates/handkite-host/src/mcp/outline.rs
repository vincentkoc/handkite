use super::compact::{self, Fallback};
use quick_xml::events::Event;
use std::collections::BTreeMap;

#[derive(Default)]
struct Node {
    role: String,
    attributes: BTreeMap<String, String>,
    children: Vec<Node>,
}

#[derive(Default)]
pub(super) struct Outline {
    pub text: String,
    pub invisible_nodes: usize,
    pub empty_wrappers: usize,
}

fn append_text(nodes: &mut Vec<Node>, text: &str) {
    if let Some(last) = nodes.last_mut().filter(|node| node.role == "#text") {
        last.attributes.get_mut("value").unwrap().push_str(text);
    } else {
        nodes.push(Node {
            role: "#text".into(),
            attributes: BTreeMap::from([("value".into(), text.into())]),
            children: Vec::new(),
        });
    }
}

/// Presentation only. Validate the entire original tree before omitting anything.
pub(super) fn project(xml: &str) -> Result<Outline, Fallback> {
    let validated = compact::project(xml)?;
    let mut reader = quick_xml::Reader::from_str(&validated);
    let mut stack: Vec<Node> = Vec::new();
    let mut roots = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| Fallback::Malformed)?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(event) | Event::Empty(event) => {
                let mut node = Node {
                    role: std::str::from_utf8(event.name().as_ref())
                        .map_err(|_| Fallback::Malformed)?
                        .into(),
                    ..Node::default()
                };
                for attribute in event.attributes() {
                    let attribute = attribute.map_err(|_| Fallback::Malformed)?;
                    let key = std::str::from_utf8(attribute.key.as_ref())
                        .map_err(|_| Fallback::Malformed)?;
                    if key != "type" {
                        node.attributes.insert(
                            key.into(),
                            attribute
                                .decode_and_unescape_value(reader.decoder())
                                .map_err(|_| Fallback::Malformed)?
                                .into_owned(),
                        );
                    }
                }
                if empty {
                    stack
                        .last_mut()
                        .map_or(&mut roots, |parent| &mut parent.children)
                        .push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(Fallback::Malformed)?;
                stack
                    .last_mut()
                    .map_or(&mut roots, |parent| &mut parent.children)
                    .push(node);
            }
            Event::Text(text) => {
                let text = text.decode().map_err(|_| Fallback::Malformed)?;
                append_text(
                    stack
                        .last_mut()
                        .map_or(&mut roots, |parent| &mut parent.children),
                    &text,
                );
            }
            Event::GeneralRef(reference) => {
                let entity = format!("&{};", reference.decode().map_err(|_| Fallback::Malformed)?);
                let text = quick_xml::escape::unescape(&entity).map_err(|_| Fallback::Malformed)?;
                append_text(
                    stack
                        .last_mut()
                        .map_or(&mut roots, |parent| &mut parent.children),
                    &text,
                );
            }
            Event::Decl(_) => {}
            Event::Eof => break,
            _ => return Err(Fallback::Unsupported),
        }
    }
    let mut result = Outline::default();
    render(&roots, 0, &mut result);
    if result.text.trim().is_empty() {
        return Err(Fallback::Unsupported);
    }
    Ok(result)
}

fn omit(node: &Node) -> bool {
    if node.role == "#text" {
        return node.attributes["value"].trim().is_empty();
    }
    node.attributes
        .get("visible")
        .is_some_and(|value| value == "false")
        && matches!(
            node.role.as_str(),
            "Other" | "StaticText" | "Icon" | "Image" | "Button" | "Cell"
        )
        && node.children.iter().all(omit)
}

fn count(node: &Node) -> usize {
    usize::from(node.role != "#text") + node.children.iter().map(count).sum::<usize>()
}

fn render(nodes: &[Node], depth: usize, result: &mut Outline) {
    let mut hidden = 0;
    for node in nodes {
        if node.role == "#text" && node.attributes["value"].trim().is_empty() {
            continue;
        }
        if omit(node) {
            hidden += count(node);
            continue;
        }
        flush_hidden(hidden, depth, result);
        hidden = 0;
        if matches!(node.role.as_str(), "AppiumAUT" | "Other") && node.attributes.is_empty() {
            result.empty_wrappers += 1;
            render(&node.children, depth, result);
            continue;
        }
        result.text.push_str(&"  ".repeat(depth));
        result.text.push_str(&node.role);
        let attributes = &node.attributes;
        let shared_name = attributes
            .get("name")
            .filter(|name| attributes.get("label") == Some(name));
        if let Some(name) = shared_name {
            result.text.push_str(" name=label=");
            result.text.push_str(&serde_json::to_string(name).unwrap());
        }
        let rect = ["x", "y", "width", "height"].map(|key| attributes.get(key));
        if rect.iter().all(Option::is_some) {
            result.text.push_str(" rect=");
            result.text.push_str(&serde_json::to_string(&rect).unwrap());
        }
        for (key, value) in attributes {
            if shared_name.is_some() && matches!(key.as_str(), "name" | "label")
                || rect.iter().all(Option::is_some)
                    && matches!(key.as_str(), "x" | "y" | "width" | "height")
            {
                continue;
            }
            result.text.push(' ');
            result.text.push_str(key);
            result.text.push('=');
            result.text.push_str(&serde_json::to_string(value).unwrap());
        }
        result.text.push('\n');
        render(&node.children, depth + 1, result);
    }
    flush_hidden(hidden, depth, result);
}

fn flush_hidden(hidden: usize, depth: usize, result: &mut Outline) {
    if hidden > 0 {
        result.invisible_nodes += hidden;
        result.text.push_str(&format!(
            "{}[omitted {hidden} invisible nodes]\n",
            "  ".repeat(depth)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_keeps_identity_states_bounds_and_exact_values() {
        let xml = r#"<AppiumAUT><XCUIElementTypeApplication name="Settings" processId="0042" x="0" y="0" width="393" height="852"><XCUIElementTypeOther><XCUIElementTypeButton name="Close" label="Close" enabled="true" visible="true" selected="false" x="338" y="489" width="38" height="38"/><XCUIElementTypeTextField value="  Café &amp; 🦊  " placeholderValue="" focused="false"/></XCUIElementTypeOther></XCUIElementTypeApplication></AppiumAUT>"#;
        let outline = project(xml).unwrap();
        assert!(
            outline
                .text
                .contains("Application rect=[\"0\",\"0\",\"393\",\"852\"] name=\"Settings\"")
        );
        assert!(outline.text.contains("processId=\"0042\""));
        assert!(outline.text.contains("Button name=label=\"Close\" rect=[\"338\",\"489\",\"38\",\"38\"] enabled=\"true\" selected=\"false\" visible=\"true\""));
        assert!(
            outline
                .text
                .contains("focused=\"false\" placeholderValue=\"\" value=\"  Café & 🦊  \"")
        );
        assert_eq!(outline.empty_wrappers, 2);
        assert_eq!(outline.invisible_nodes, 0);
    }

    #[test]
    fn hidden_parent_never_discards_visible_child_or_secure_context() {
        let outline = project(r#"<AppiumAUT><XCUIElementTypeOther visible="false"><XCUIElementTypeButton visible="true" label="Keep"/></XCUIElementTypeOther><XCUIElementTypeSecureTextField visible="false"/><XCUIElementTypeAlert visible="false"/><XCUIElementTypeButton visible="false" label="Hidden"/></AppiumAUT>"#).unwrap();
        assert!(outline.text.contains("Other visible=\"false\"\n  Button"));
        assert!(outline.text.contains("label=\"Keep\""));
        assert!(outline.text.contains("SecureTextField visible=\"false\""));
        assert!(outline.text.contains("Alert visible=\"false\""));
        assert!(!outline.text.contains("label=\"Hidden\""));
        assert!(outline.text.contains("[omitted 1 invisible nodes]"));
        assert_eq!(outline.invisible_nodes, 1);
    }

    #[test]
    fn duplicate_labels_and_text_nodes_are_not_merged() {
        let outline = project(r#"<AppiumAUT><XCUIElementTypeButton label="Same"/><XCUIElementTypeButton label="Same"/> A &amp; B </AppiumAUT>"#).unwrap();
        assert_eq!(outline.text.matches("Button label=\"Same\"").count(), 2);
        assert!(outline.text.contains("#text value=\" A & B \""));
    }

    #[test]
    fn hidden_malformed_or_unknown_content_still_refuses_projection() {
        assert!(project("<AppiumAUT/>").is_err());
        let modal = project(r#"<AppiumAUT><XCUIElementTypePopover visible="false"/><XCUIElementTypeMenu visible="false"/></AppiumAUT>"#).unwrap();
        assert!(modal.text.contains("Popover visible=\"false\""));
        assert!(modal.text.contains("Menu visible=\"false\""));
        assert!(project(r#"<AppiumAUT><XCUIElementTypeOther visible="false"><XCUIElementTypeButton label="x" label="y"/></XCUIElementTypeOther></AppiumAUT>"#).is_err());
        assert!(
            project(
                r#"<AppiumAUT><XCUIElementTypeOther visible="false" unknown="x"/></AppiumAUT>"#
            )
            .is_err()
        );
    }
}
