use super::AdapterError;
use crate::wda_http::HttpResponse;
use quick_xml::events::Event;

// The 5s observation window leaves 3.5s after ~1.5s of other capture work.
// Measured visibility cost is ~9.1ms/node (17.2s / 1881 nodes), allowing ~384
// nodes. Round down to 350 for ~0.3s of margin. This selects work only: the
// original bracket/deadline still decides whether a capture is admissible.
const VISIBILITY_NODE_LIMIT: usize = 350;

#[derive(Clone)]
pub(super) struct Cost {
    pub nodes: usize,
    web_view: bool,
}

impl Cost {
    pub fn within_visibility_budget(&self) -> bool {
        self.nodes <= VISIBILITY_NODE_LIMIT
    }

    pub fn enrich_visibility(&self) -> bool {
        self.within_visibility_budget() && !self.web_view
    }
}

pub(super) fn cost(response: &HttpResponse) -> Result<Cost, AdapterError> {
    if response.status != 200 {
        return Err(AdapterError);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    let xml = value["value"].as_str().ok_or(AdapterError)?;
    xml_cost(xml)
}

pub(super) fn xml_cost(xml: &str) -> Result<Cost, AdapterError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut cost = Cost {
        nodes: 0,
        web_view: false,
    };
    let (mut depth, mut roots) = (0usize, 0usize);
    loop {
        let event = reader.read_event().map_err(|_| AdapterError)?;
        let opens = matches!(&event, Event::Start(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                let name = node.name();
                let kind = std::str::from_utf8(name.as_ref()).map_err(|_| AdapterError)?;
                if kind.starts_with("XCUIElementType") {
                    cost.nodes += 1;
                    cost.web_view |= kind == "XCUIElementTypeWebView";
                } else if kind != "AppiumAUT" {
                    return Err(AdapterError);
                }
                for attr in node.attributes() {
                    attr.map_err(|_| AdapterError)?
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|_| AdapterError)?;
                }
                if depth == 0 {
                    roots += 1;
                }
                if opens {
                    depth += 1;
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or(AdapterError)?,
            Event::Text(text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
            Event::Eof if depth == 0 && roots == 1 && cost.nodes > 0 => return Ok(cost),
            _ => return Err(AdapterError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CaptureSource, OUTLINE_SOURCE, SCREEN_SOURCE, WdaAdapter};
    use crate::wda_action::WdaRequest;
    use crate::wda_http::HttpResponse;
    use crate::wda_transport::WdaError;
    use base64::Engine;
    use handkite_core::protocol::HardwareId;
    use serde_json::json;
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    struct CityReads {
        calls: RefCell<Vec<String>>,
        xml: String,
        cheap_delay: Duration,
        visibility_delay: Duration,
        change: u8,
    }

    impl WdaRequest for CityReads {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            assert_eq!(method, "GET");
            assert!(body.is_empty());
            self.calls.borrow_mut().push(path.to_owned());
            let after = self
                .calls
                .borrow()
                .iter()
                .filter(|p| p.as_str() == path)
                .count()
                > 1;
            let value = match path {
                "/screenshot" => json!(base64::engine::general_purpose::STANDARD.encode(
                    if after && self.change == 3 {
                        b"\x89PNG\r\n\x1a\nchanged-pixels".as_slice()
                    } else {
                        b"\x89PNG\r\n\x1a\nfixture-pixels".as_slice()
                    }
                )),
                "/wda/activeAppInfo" => {
                    json!({"bundleId": if after && self.change == 1 { "other-app" } else { "fixture-native-app" }})
                }
                "/wda/locked" => json!(after && self.change == 2),
                SCREEN_SOURCE => {
                    tokio::time::sleep(self.visibility_delay).await;
                    json!(self.xml.replace(" name=", " visible=\"true\" name="))
                }
                OUTLINE_SOURCE => {
                    tokio::time::sleep(self.cheap_delay).await;
                    json!(self.xml)
                }
                _ => panic!("unexpected request"),
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"value":value})).unwrap(),
            })
        }
    }

    fn tree(nodes: usize) -> String {
        let mut tree = String::from("<AppiumAUT><XCUIElementTypeApplication>");
        for index in 0..nodes - 1 {
            tree.push_str(&format!(
                "<XCUIElementTypeCell name=\"fixture-city-{index}\"/>"
            ));
        }
        tree.push_str("</XCUIElementTypeApplication></AppiumAUT>");
        tree
    }

    fn reads(nodes: usize) -> CityReads {
        CityReads {
            calls: RefCell::new(Vec::new()),
            xml: tree(nodes),
            cheap_delay: Duration::ZERO,
            visibility_delay: Duration::from_millis(17_200),
            change: 0,
        }
    }

    fn profile(xml: &str) -> Result<super::Cost, super::AdapterError> {
        super::cost(&HttpResponse {
            status: 200,
            body: serde_json::to_vec(&json!({"value": xml})).unwrap(),
        })
    }

    #[test]
    fn visibility_work_bound_and_web_content_are_cost_evidence_only() {
        for (nodes, enrich) in [
            (1, true),
            (64, true),
            (65, true),
            (350, true),
            (351, false),
            (1881, false),
        ] {
            let cost = profile(&tree(nodes)).unwrap();
            assert_eq!(cost.nodes, nodes);
            assert_eq!(cost.enrich_visibility(), enrich);
        }
        assert!(
            !profile(
                "<XCUIElementTypeApplication><XCUIElementTypeWebView/></XCUIElementTypeApplication>"
            )
            .unwrap()
            .enrich_visibility()
        );
    }

    #[test]
    fn incomplete_or_unknown_hierarchies_never_select_visibility_enrichment() {
        for xml in [
            "",
            "fixture-text",
            "<AppiumAUT/>",
            "<XCUIElementTypeApplication>",
            "<XCUIElementTypeApplication></wrong>",
            "<XCUIElementTypeApplication/><XCUIElementTypeCell/>",
            "<unknown><XCUIElementTypeApplication/></unknown>",
            "<XCUIElementTypeApplication name=\"bad &entity;\"/>",
            "<XCUIElementTypeApplication name=\"one\" name=\"two\"/>",
        ] {
            assert!(profile(xml).is_err());
        }
        for body in [
            b"invalid-json".as_slice(),
            br#"{"value":null}"#,
            br#"{"value":""}"#,
            br#"{}"#,
        ] {
            assert!(
                super::cost(&HttpResponse {
                    status: 200,
                    body: body.to_vec()
                })
                .is_err()
            );
        }
        assert!(
            super::cost(&HttpResponse {
                status: 500,
                body: serde_json::to_vec(&json!({"value":tree(1)})).unwrap()
            })
            .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn city_outline_keeps_the_complete_tree_inside_the_original_budget() {
        let reads = reads(1001);
        let start = tokio::time::Instant::now();
        let ((raw, _, _), presentation_only) = WdaAdapter::capture_using_scoped(
            &reads,
            &HardwareId::new("fixture-device").unwrap(),
            CaptureSource::Outline(SCREEN_SOURCE),
            17,
            &Instant::now,
            &|| Ok(21),
            None,
            Some((start + Duration::from_secs(5)).into_std()),
        )
        .await
        .expect("a passive outline must not pay per-city visibility costs");
        assert!(presentation_only);
        assert_eq!(raw.accessibility_tree, reads.xml);
        assert!(raw.accessibility_tree.contains("fixture-city-999"));
        assert!(tokio::time::Instant::now() - start < Duration::from_secs(5));
        assert!(
            !reads
                .calls
                .borrow()
                .iter()
                .any(|path| path == SCREEN_SOURCE)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn small_outline_publishes_only_the_enriched_tree_in_the_original_bracket() {
        for nodes in [54, 350] {
            let mut reads = reads(nodes);
            reads.visibility_delay = Duration::from_millis(400);
            let ((raw, _, _), presentation_only) = WdaAdapter::capture_using_scoped(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                CaptureSource::Outline(SCREEN_SOURCE),
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                None,
            )
            .await
            .unwrap();
            assert!(!presentation_only);
            assert!(raw.accessibility_tree.contains("visible=\"true\""));
            let calls = reads.calls.borrow();
            assert_eq!(
                calls.iter().map(String::as_str).collect::<Vec<_>>(),
                [
                    "/screenshot",
                    "/wda/activeAppInfo",
                    "/wda/locked",
                    OUTLINE_SOURCE,
                    SCREEN_SOURCE,
                    "/screenshot",
                    "/wda/activeAppInfo",
                    "/wda/locked",
                ]
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn enrichment_spends_the_same_deadline_and_never_restarts_it() {
        let mut reads = reads(54);
        reads.cheap_delay = Duration::from_secs(3);
        reads.visibility_delay = Duration::from_secs(3);
        let start = tokio::time::Instant::now();
        assert!(
            WdaAdapter::capture_using_scoped(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                CaptureSource::Outline(SCREEN_SOURCE),
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                Some((start + Duration::from_secs(5)).into_std()),
            )
            .await
            .is_err()
        );
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn cheap_outline_keeps_image_app_and_lock_coherence_checks() {
        for change in 1..=3 {
            let mut reads = reads(1001);
            reads.change = change;
            assert!(
                WdaAdapter::capture_using_scoped(
                    &reads,
                    &HardwareId::new("fixture-device").unwrap(),
                    CaptureSource::Outline(SCREEN_SOURCE),
                    17,
                    &Instant::now,
                    &|| Ok(21),
                    None,
                    None,
                )
                .await
                .is_err()
            );
        }
    }
}
