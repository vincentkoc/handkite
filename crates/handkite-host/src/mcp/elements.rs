//! Presentation handles are session-local. Native admission must revalidate a target.
use crate::wire::WireObservation;
use handkite_core::protocol::{DeviceKey, DeviceState, Revision};
use quick_xml::{Reader, events::Event};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Element {
    pub id: String,
    pub role: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_source: Option<String>,
    #[serde(skip_serializing)]
    target_id: Option<String>,
    pub label: String,
    pub rect: [f64; 4],
    pub value: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub value_truncated: bool,
    pub enabled: bool,
    #[serde(skip_serializing)]
    name: String,
}

impl Element {
    fn key(&self) -> String {
        // Value and enabled state may change while the element stays put.
        serde_json::to_string(&(
            &self.source,
            &self.role,
            &self.name,
            &self.label,
            &self.rect,
        ))
        .unwrap()
    }
}

struct Baseline {
    revision: Revision,
    device: DeviceKey,
    state: DeviceState,
    elements: Vec<Element>,
}

#[derive(Default)]
pub(super) struct Cache {
    last: Option<Baseline>,
    next_id: u64,
}

impl Cache {
    pub fn clear(&mut self) {
        self.last = None;
    }

    pub fn rects(&self, revision: Revision) -> Vec<[f64; 4]> {
        self.last
            .iter()
            .filter(|baseline| baseline.revision == revision)
            .flat_map(|baseline| baseline.elements.iter().map(|element| element.rect))
            .collect()
    }

    pub fn project(
        &mut self,
        observation: &WireObservation,
        since: Option<Revision>,
    ) -> Result<Value, &'static str> {
        let mut elements = parse(&observation.accessibility_tree)?;
        let previous = self
            .last
            .as_ref()
            .filter(|old| old.device == observation.device && old.state == observation.state);
        let mut old_keys: BTreeMap<String, Vec<&Element>> = BTreeMap::new();
        if let Some(previous) = previous {
            for element in &previous.elements {
                old_keys.entry(element.key()).or_default().push(element);
            }
        }
        let mut counts = BTreeMap::new();
        for element in &elements {
            *counts.entry(element.key()).or_insert(0usize) += 1;
        }
        for element in &mut elements {
            let key = element.key();
            let unique = old_keys
                .get(&key)
                .filter(|old| old.len() == 1 && counts[&key] == 1);
            let host_id = element
                .target_id
                .as_deref()
                .or_else(|| (element.source == "ocr").then_some(element.name.as_str()));
            element.id = if host_id.is_some_and(|id| {
                id.len() == 34
                    && id.starts_with("t_")
                    && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                host_id.unwrap().to_owned()
            } else if let Some(old) = unique.filter(|old| old[0].id.starts_with('e')) {
                old[0].id.clone()
            } else {
                self.next_id = self.next_id.checked_add(1).ok_or("element_id_exhausted")?;
                format!("e{}", self.next_id)
            };
        }
        let mut payload = json!({"metadata":super::response::metadata(observation), "tree_format":"elements", "tree_format_version":1});
        if let Some(old) = previous
            .filter(|old| Some(old.revision) == since && observation.revision.0 > old.revision.0)
        {
            let old_ids: BTreeMap<_, _> = old.elements.iter().map(|e| (&e.id, e)).collect();
            let new_ids: BTreeMap<_, _> = elements.iter().map(|e| (&e.id, e)).collect();
            let added: Vec<_> = elements
                .iter()
                .filter(|e| !old_ids.contains_key(&e.id))
                .collect();
            let removed: Vec<_> = old
                .elements
                .iter()
                .filter(|e| !new_ids.contains_key(&e.id))
                .map(|e| &e.id)
                .collect();
            let changed: Vec<_> = elements
                .iter()
                .filter(|e| old_ids.get(&e.id).is_some_and(|old| *old != *e))
                .collect();
            payload["elements_delta"] = json!({"base_revision":old.revision, "added":added, "removed":removed, "changed":changed,
                "order":elements.iter().map(|e| &e.id).collect::<Vec<_>>()});
        } else {
            payload["elements"] = json!(elements);
            if since.is_some() {
                payload["tree_delta_reset"] = json!("baseline_unavailable");
            }
        }
        self.last = Some(Baseline {
            revision: observation.revision,
            device: observation.device.clone(),
            state: observation.state.clone(),
            elements,
        });
        Ok(payload)
    }
}

fn parse(xml: &str) -> Result<Vec<Element>, &'static str> {
    // Reuse the strict XML grammar, size/depth limits and DTD rejection.
    let xml = super::compact::project(xml).map_err(|_| "unsupported_tree")?;
    let mut reader = Reader::from_str(&xml);
    let mut elements = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| "malformed_tree")?;
        match event {
            Event::Start(event) | Event::Empty(event) => {
                let mut attrs = BTreeMap::new();
                for attr in event.attributes() {
                    let attr = attr.map_err(|_| "malformed_tree")?;
                    attrs.insert(
                        String::from_utf8(attr.key.as_ref().to_vec())
                            .map_err(|_| "malformed_tree")?,
                        attr.decode_and_unescape_value(reader.decoder())
                            .map_err(|_| "malformed_tree")?
                            .into_owned(),
                    );
                }
                if attrs.get("visible").map(String::as_str) != Some("true") {
                    continue;
                }
                let coordinate = |key: &str| {
                    attrs
                        .get(key)
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|v| v.is_finite())
                };
                let (Some(x), Some(y), Some(w), Some(h)) = (
                    coordinate("x"),
                    coordinate("y"),
                    coordinate("width"),
                    coordinate("height"),
                ) else {
                    continue;
                };
                if w <= 0. || h <= 0. {
                    continue;
                }
                let role = std::str::from_utf8(event.name().as_ref())
                    .map_err(|_| "malformed_tree")?
                    .to_owned();
                let value = if role == "SecureTextField" {
                    "".into()
                } else {
                    attrs.get("value").cloned().unwrap_or_default()
                };
                let truncated = value.chars().count() > 512;
                elements.push(Element {
                    id: String::new(),
                    source: attrs.get("source").cloned().unwrap_or_else(|| "xml".into()),
                    input_source: attrs.get("input_source").cloned(),
                    target_id: attrs.get("target_id").cloned(),
                    role,
                    label: attrs.get("label").cloned().unwrap_or_default(),
                    name: attrs.get("name").cloned().unwrap_or_default(),
                    rect: [x, y, w, h],
                    value: value.chars().take(512).collect(),
                    value_truncated: truncated,
                    enabled: attrs.get("enabled").map(String::as_str) == Some("true"),
                });
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(role: &str, label: &str, value: &str, x: i32) -> String {
        format!(
            "<XCUIElementType{role} visible='true' enabled='true' label='{label}' value='{value}' x='{x}' y='1' width='20' height='20'/>"
        )
    }
    fn observation(rev: u64, nodes: &str) -> WireObservation {
        serde_json::from_value(json!({"device":"fixture", "revision":rev,"window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"lock":"unlocked","wda_ready":true,"foreground_app":"com.example.fixture"},
        "accessibility_tree":format!("<AppiumAUT>{nodes}</AppiumAUT>"),"screenshot_format":"png","screenshot_base64":"AQID"})).unwrap()
    }
    #[test]
    fn ocr_elements_keep_the_host_target_id_and_provenance() {
        let mut cache = Cache::default();
        let id = "t_0123456789abcdef0123456789abcdef";
        let xml = format!(
            "<XCUIElementTypeStaticText source='ocr' name='{id}' label='District' visible='true' enabled='true' x='10' y='20' width='80' height='20'/>"
        );
        let first = cache.project(&observation(1, &xml), None).unwrap();
        assert_eq!(first["elements"][0]["id"], id);
        assert_eq!(first["elements"][0]["source"], "ocr");
        let next = cache
            .project(&observation(2, &xml), Some(Revision(1)))
            .unwrap();
        assert_eq!(next["elements_delta"]["added"], json!([]));
        assert_eq!(next["elements_delta"]["changed"], json!([]));
    }
    #[test]
    fn xml_rows_keep_native_provenance_with_explicit_ocr_input_handles() {
        let mut cache = Cache::default();
        let id = "t_0123456789abcdef0123456789abcdef";
        let xml = format!(
            "<XCUIElementTypeStaticText target_id='{id}' input_source='ocr' name='native-name' label='District' visible='true' enabled='true' x='10' y='20' width='80' height='20'/>"
        );
        let first = cache.project(&observation(1, &xml), None).unwrap();
        assert_eq!(first["elements"][0]["id"], id);
        assert_eq!(first["elements"][0]["source"], "xml");
        assert_eq!(first["elements"][0]["input_source"], "ocr");
        let next = cache
            .project(&observation(2, &xml), Some(Revision(1)))
            .unwrap();
        assert_eq!(next["elements_delta"]["changed"], json!([]));
        assert_eq!(next["elements_delta"]["added"], json!([]));
        let ordinary = xml.replace(&format!("target_id='{id}' input_source='ocr' "), "");
        let plain = cache
            .project(&observation(3, &ordinary), Some(Revision(2)))
            .unwrap();
        assert_eq!(plain["elements_delta"]["removed"], json!([id]));
        assert!(
            plain["elements_delta"]["added"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with('e')
        );
    }
    #[test]
    fn stable_ids_survive_reordering_and_values_produce_changed_elements() {
        let mut cache = Cache::default();
        let a = node("TextField", "Editor", "first", 1);
        let b = node("Button", "Done", "", 2);
        let first = cache
            .project(&observation(1, &format!("{a}{b}")), None)
            .unwrap();
        let second = cache
            .project(
                &observation(
                    2,
                    &format!("{b}{}", node("TextField", "Editor", "second", 1)),
                ),
                Some(Revision(1)),
            )
            .unwrap();
        let d = &second["elements_delta"];
        assert_eq!(d["added"], json!([]));
        assert_eq!(d["removed"], json!([]));
        assert_eq!(d["changed"][0]["id"], first["elements"][0]["id"]);
        assert_eq!(d["changed"][0]["value"], "second");
        assert_eq!(d["order"][0], first["elements"][1]["id"]);
    }
    #[test]
    fn ambiguous_or_replaced_elements_do_not_inherit_handles() {
        let mut cache = Cache::default();
        let a = node("Cell", "Row", "", 1);
        let first = cache.project(&observation(1, &a), None).unwrap();
        let next = cache
            .project(&observation(2, &format!("{a}{a}")), Some(Revision(1)))
            .unwrap();
        assert_eq!(
            next["elements_delta"]["removed"],
            json!([first["elements"][0]["id"]])
        );
        assert_eq!(next["elements_delta"]["added"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn preserve_visible_descendants_but_redact_secure_values() {
        let hidden = format!(
            "<XCUIElementTypeOther visible='false'>{}</XCUIElementTypeOther>",
            node("Button", "Hidden", "", 1)
        );
        let parsed = parse(&format!(
            "<AppiumAUT>{hidden}{}</AppiumAUT>",
            node("SecureTextField", "Password", "secret", 2)
        ))
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].value, "");
        assert!(parse("<!DOCTYPE x><AppiumAUT/>").is_err());
    }
    #[test]
    fn wrapper_visibility_change_does_not_remove_a_visible_child() {
        let mut cache = Cache::default();
        let child = node("Button", "Done", "", 1);
        let before = format!("<XCUIElementTypeOther visible='true'>{child}</XCUIElementTypeOther>");
        let after = before.replace("Other visible='true'", "Other visible='false'");
        cache.project(&observation(1, &before), None).unwrap();
        let next = cache
            .project(&observation(2, &after), Some(Revision(1)))
            .unwrap();
        assert_eq!(next["elements_delta"]["removed"], json!([]));
        assert_eq!(next["elements_delta"]["changed"], json!([]));
        assert_eq!(next["elements_delta"]["added"], json!([]));
    }

    #[test]
    fn unknown_revision_or_app_change_resets_delta() {
        let mut cache = Cache::default();
        let a = node("Button", "Done", "", 1);
        cache.project(&observation(1, &a), None).unwrap();
        let first = cache
            .project(&observation(2, &a), Some(Revision(99)))
            .unwrap();
        assert!(first.get("elements").is_some());
        let mut other = observation(3, &a);
        other.state.foreground_app = Some("com.example.other".into());
        let next = cache.project(&other, Some(Revision(2))).unwrap();
        assert!(next.get("elements_delta").is_none());
        assert_ne!(first["elements"][0]["id"], next["elements"][0]["id"]);
    }
}
