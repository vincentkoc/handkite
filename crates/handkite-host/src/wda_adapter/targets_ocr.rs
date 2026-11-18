//! OCR targets are tap-only, backed by current image evidence, not native IDs.
use super::*;
use crate::wda_adapter::{ocr, targets_pixels::Pixels};

fn to_points([x, y, w, h]: [f64; 4], [sx, sy]: [f64; 2]) -> [f64; 4] {
    [x / sx, y / sy, w / sx, h / sy]
}

fn attrs(rect: [f64; 4]) -> Attributes {
    ["x", "y", "width", "height"]
        .into_iter()
        .zip(rect)
        .map(|(k, v)| (k.into(), v.to_string()))
        .collect()
}

#[derive(Clone)]
pub(super) struct OcrBinding {
    text: ocr::Text,
    mapping: OcrMapping,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OcrMapping {
    viewport: [f64; 4],
    dimensions: [u32; 2],
    scale: f64,
}

impl OcrBinding {
    fn matches(&self, mapping: OcrMapping, rows: &[ocr::Text]) -> bool {
        if self.mapping != mapping || !ocr::safe_context(rows) {
            return false;
        }
        let Some(matched) = ocr::matching_text(rows, &self.text) else {
            return false;
        };
        // Dispatch keeps the original center; even permitted drift must contain it.
        (0..2).all(|axis| {
            let point = self.text.rect[axis] + self.text.rect[axis + 2] / 2.;
            point >= matched.rect[axis] && point < matched.rect[axis] + matched.rect[axis + 2]
        })
    }
}

#[derive(Debug, PartialEq)]
enum Placement {
    Synthetic,
    Attach(usize),
    Suppress,
}

const TEXT_KINDS: &[&str] = &[
    "XCUIElementTypeStaticText",
    "XCUIElementTypeCell",
    "XCUIElementTypeButton",
    "XCUIElementTypeMenuItem",
    "XCUIElementTypeLink",
    "XCUIElementTypeTextField",
    "XCUIElementTypeTextView",
    "XCUIElementTypeSearchField",
    "XCUIElementTypePickerWheel",
];

fn matching_text_area(node: &Attributes, text: &str, rect: [f64; 4]) -> Option<f64> {
    if !TEXT_KINDS.contains(&node.get("type")?.as_str())
        || !["label", "name", "value"].iter().any(|key| {
            node.get(*key)
                .is_some_and(|v| v.split_whitespace().collect::<String>() == text)
        })
    {
        return None;
    }
    let r = ["x", "y", "width", "height"]
        .iter()
        .map(|key| {
            node.get(*key)?
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
        })
        .collect::<Option<Vec<_>>>()?;
    // Require complete glyph containment, allowing only subpixel OCR rounding.
    (r[2] > 0.
        && r[3] > 0.
        && rect[0] >= r[0] - 1.
        && rect[1] >= r[1] - 1.
        && rect[0] + rect[2] <= r[0] + r[2] + 1.
        && rect[1] + rect[3] <= r[1] + r[3] + 1.)
        .then_some(r[2] * r[3])
}

// OCR may establish visible text, not a native element's enabled/focus state.
fn placement(
    nodes: &[Attributes],
    row: &ocr::Text,
    scale: [f64; 2],
    entries: &[Bound],
) -> Placement {
    let rect = to_points(row.rect, scale);
    let text = row.text.split_whitespace().collect::<String>();
    if text.is_empty() {
        return Placement::Suppress;
    }
    let mut matches = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some(area) = matching_text_area(node, &text, rect) else {
            continue;
        };
        if node.get("enabled").map(String::as_str) == Some("false")
            || node.get("visible").map(String::as_str) == Some("false")
        {
            return Placement::Suppress;
        }
        matches.push((index, area));
    }
    matches.sort_by(|a, b| a.1.total_cmp(&b.1));
    let Some(&(index, area)) = matches.first() else {
        return Placement::Synthetic;
    };
    if matches
        .get(1)
        .is_some_and(|(_, other)| (area - other).abs() < 0.01)
        || entries.iter().any(|entry| entry.attrs == nodes[index])
    {
        Placement::Suppress
    } else {
        Placement::Attach(index)
    }
}

fn presentation_xml(
    xml: &str,
    attachments: &std::collections::BTreeMap<usize, String>,
    extra: &[u8],
) -> Result<String, AdapterError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut writer = quick_xml::Writer::new(Vec::new());
    let mut index = 0;
    loop {
        let event = reader.read_event().map_err(|_| AdapterError)?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                let id = attachments.get(&index);
                index += 1;
                let mut out = node.to_owned();
                if let Some(id) = id {
                    out.clear_attributes();
                    for attr in node.attributes() {
                        let attr = attr.map_err(|_| AdapterError)?;
                        if !matches!(
                            attr.key.as_ref(),
                            b"visible" | b"target_id" | b"input_source"
                        ) {
                            out.push_attribute(attr);
                        }
                    }
                    out.push_attribute(("visible", "true"));
                    out.push_attribute(("target_id", id.as_str()));
                    out.push_attribute(("input_source", "ocr"));
                }
                writer
                    .write_event(if empty {
                        Event::Empty(out)
                    } else {
                        Event::Start(out)
                    })
                    .map_err(|_| AdapterError)?;
            }
            Event::End(node) if node.name().as_ref() == b"AppiumAUT" => {
                use std::io::Write;
                writer
                    .get_mut()
                    .write_all(extra)
                    .map_err(|_| AdapterError)?;
                writer
                    .write_event(Event::End(node))
                    .map_err(|_| AdapterError)?;
            }
            Event::Eof => break,
            other => writer
                .write_event(other.into_owned())
                .map_err(|_| AdapterError)?,
        }
    }
    String::from_utf8(writer.into_inner()).map_err(|_| AdapterError)
}

pub(super) fn augment(
    cache: &mut ocr::Cache,
    identity: &str,
    xml: &str,
    before: &ScreenImage,
    after: &ScreenImage,
    native_scale: f64,
    entries: &mut Vec<Bound>,
) -> Result<String, AdapterError> {
    let _span = crate::timing::Span::new("target_ocr_merge");
    let viewport = super::super::targets_pixels::viewport(xml)?;
    let first = Pixels::decode(&before.bytes)?;
    let last = Pixels::decode(&after.bytes)?;
    let rows = [
        cache.recognize(identity, &first)?,
        cache.recognize(identity, &last)?,
    ];
    merge_rows(
        identity,
        xml,
        OcrMapping {
            viewport,
            dimensions: [last.width, last.height],
            scale: native_scale,
        },
        &first,
        &last,
        rows,
        entries,
    )
}

fn merge_rows(
    identity: &str,
    xml: &str,
    mapping: OcrMapping,
    first: &Pixels,
    last: &Pixels,
    rows: [Vec<ocr::Text>; 2],
    entries: &mut Vec<Bound>,
) -> Result<String, AdapterError> {
    let [before_rows, rows] = rows;
    if [first.width, first.height] != mapping.dimensions
        || [last.width, last.height] != mapping.dimensions
        || !ocr::safe_context(&before_rows)
        || !ocr::safe_context(&rows)
    {
        return Err(AdapterError);
    }
    let viewport = mapping.viewport;
    for pixels in [first, last] {
        pixels.validate_mapping(viewport, mapping.scale, [viewport[2], viewport[3]])?;
    }
    let scale = [
        last.width as f64 / viewport[2],
        last.height as f64 / viewport[3],
    ];
    let nodes = xml_nodes(xml)?;
    let mut attachments = std::collections::BTreeMap::new();
    let mut writer = quick_xml::Writer::new(Vec::new());
    let context = std::sync::Arc::new(rows.clone());
    let placements: Vec<_> = rows
        .iter()
        .map(|row| placement(&nodes, row, scale, entries))
        .collect();
    let mut claims = std::collections::BTreeMap::new();
    for placement in &placements {
        if let Placement::Attach(index) = placement {
            *claims.entry(*index).or_insert(0usize) += 1;
        }
    }
    let mut after_budget = 0;
    for (row, placement) in rows.into_iter().zip(placements) {
        if placement == Placement::Suppress
            || matches!(placement, Placement::Attach(index) if claims[&index] != 1)
            || !ocr::same_text(&before_rows, &row)
            || !ocr::same_text(context.as_ref(), &row)
        {
            continue;
        }
        let rect = to_points(row.rect, scale);
        let [x, y, w, h] = rect;
        let left = (x - 16.).max(0.);
        let top = (y - 16.).max(0.);
        let a = attrs([
            left,
            top,
            (x + w + 16.).min(viewport[2]) - left,
            (y + h + 16.).min(viewport[3]) - top,
        ]);
        // Retain pixels for effect readback only; changing background pixels do
        // not invalidate text independently recognized in both captures.
        let Ok(last_region) = last.region(viewport, &a, &mut after_budget) else {
            continue;
        };
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        hash.update(identity.as_bytes());
        hash.update(row.text.as_bytes());
        for v in rect {
            hash.update(&((v * 2.).round() as i64).to_le_bytes());
        }
        let id = format!(
            "t_{}",
            hash.finish().as_ref()[..16]
                .iter()
                .map(|v| format!("{v:02x}"))
                .collect::<String>()
        );
        let mut node = quick_xml::events::BytesStart::new("XCUIElementTypeStaticText");
        for (k, v) in &attrs(rect) {
            node.push_attribute((k.as_str(), v.as_str()));
        }
        node.push_attribute(("label", row.text.as_str()));
        node.push_attribute(("name", id.as_str()));
        node.push_attribute(("source", "ocr"));
        node.push_attribute(("visible", "true"));
        node.push_attribute(("enabled", "true"));
        if let Placement::Attach(index) = placement {
            attachments.insert(index, id.clone());
        } else {
            writer
                .write_event(Event::Empty(node))
                .map_err(|_| AdapterError)?;
        }
        entries.push(Bound {
            public: ElementTarget {
                target_id: id,
                source: "ocr".into(),
                kind: "Text".into(),
                label: row.text.clone(),
                can_type: false,
                text_only: false,
                rect,
                value: None,
                value_truncated: false,
            },
            native: String::new(),
            attrs: a,
            rich: Value::Null,
            region: Some(last_region),
            focused: None,
            ocr: Some(OcrBinding { text: row, mapping }),
            ocr_before: Some(context.clone()),
            mapping: None,
        });
    }
    presentation_xml(xml, &attachments, writer.get_ref())
}

fn window_viewport(window: &Value) -> Result<[f64; 4], AdapterError> {
    let mut rect = [0.; 4];
    for (index, key) in ["x", "y", "width", "height"].iter().enumerate() {
        rect[index] = window[*key]
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or(AdapterError)?;
    }
    if rect[0] != 0.
        || rect[1] != 0.
        || rect[2] <= 0.
        || rect[3] <= 0.
        || rect[2] > 10_000.
        || rect[3] > 10_000.
    {
        return Err(AdapterError);
    }
    Ok(rect)
}

async fn unsafe_context_absent(wda: &impl WdaRequest, session: &str) -> Result<(), AdapterError> {
    let mut terms = vec![
        "type IN {'XCUIElementTypeAlert','XCUIElementTypeSheet','XCUIElementTypeSecureTextField'}"
            .to_owned(),
    ];
    for attribute in ["name", "label", "value", "placeholderValue"] {
        for marker in [
            "proud-lock",
            "passcode-view",
            "call-to-action-label",
            "Enter Passcode",
        ] {
            terms.push(format!("{attribute} CONTAINS '{marker}'"));
        }
    }
    let body = serde_json::to_vec(&json!({"using":"predicate string","value":terms.join(" OR ")}))
        .map_err(|_| AdapterError)?;
    let rows = value(
        wda,
        session,
        "POST",
        &format!("/session/{session}/elements"),
        &body,
    )
    .await?;
    if !rows.as_array().is_some_and(Vec::is_empty) {
        return Err(AdapterError);
    }
    Ok(())
}

async fn native_text_contradiction_absent(
    wda: &impl WdaRequest,
    session: &str,
    binding: &OcrBinding,
    current: &ocr::Text,
) -> Result<(), AdapterError> {
    let text = binding.text.text.split_whitespace().collect::<String>();
    let seed = serde_json::to_string(&text.chars().next().ok_or(AdapterError)?.to_string())
        .map_err(|_| AdapterError)?;
    let kinds = TEXT_KINDS
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(",");
    // A character filter includes every whitespace-normalized match. Compare
    // complete text and glyph containment locally; never filter hidden/disabled rows.
    let predicate = format!(
        "type IN {{{kinds}}} AND (label CONTAINS {seed} OR name CONTAINS {seed} OR value CONTAINS {seed})"
    );
    let result = query_predicate(wda, session, &predicate).await?;
    let rows = result
        .as_array()
        .filter(|rows| rows.len() <= 2048)
        .ok_or(AdapterError)?;
    let scale = [
        binding.mapping.dimensions[0] as f64 / binding.mapping.viewport[2],
        binding.mapping.dimensions[1] as f64 / binding.mapping.viewport[3],
    ];
    let rect = to_points(current.rect, scale);
    let mut identities = BTreeSet::new();
    for row in rows {
        if !identities.insert(id(row)?) {
            return Err(AdapterError);
        }
        let kind = row["type"]
            .as_str()
            .filter(|kind| TEXT_KINDS.contains(kind))
            .ok_or(AdapterError)?;
        let enabled = row["enabled"].as_bool().ok_or(AdapterError)?;
        let visible = row["displayed"].as_bool().ok_or(AdapterError)?;
        let mut node = Attributes::new();
        node.insert("type".into(), kind.into());
        for (key, field) in [
            ("label", "label"),
            ("name", "attribute/name"),
            ("value", "attribute/value"),
        ] {
            match row.get(field) {
                Some(Value::String(value)) if value.len() <= 16_384 => {
                    node.insert(key.into(), value.clone());
                }
                Some(Value::Null) => {}
                _ => return Err(AdapterError),
            }
        }
        for key in ["x", "y", "width", "height"] {
            let value = row["rect"][key]
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or(AdapterError)?;
            node.insert(key.into(), value.to_string());
        }
        if (!enabled || !visible) && matching_text_area(&node, &text, rect).is_some() {
            return Err(AdapterError);
        }
    }
    Ok(())
}

async fn evidence(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
) -> Result<(OcrMapping, ScreenImage, Pixels), AdapterError> {
    let _span = crate::timing::Span::new("ocr_current_context");
    let display_id = super::super::display::selection(wda, session).await?;
    app_matches(wda, session, app).await?;
    // Preserve the full-tree unsafe-context refusals without computing every
    // unrelated node's visibility. No visibility filter may hide a secure field.
    unsafe_context_absent(wda, session).await?;
    let image = screenshot(wda).await?;
    let pixels = Pixels::decode(&image.bytes)?;
    let window_path = format!("/session/{session}/window/rect");
    let (screen, window) = tokio::join!(
        super::super::display::screen_scale(wda, display_id),
        value(wda, session, "GET", &window_path, b"")
    );
    let screen = screen?;
    let viewport = window_viewport(&window?)?;
    let scale = screen;
    pixels.validate_mapping(viewport, scale, [viewport[2], viewport[3]])?;
    app_matches(wda, session, app).await?;
    super::super::display::verify_selection(wda, session, display_id).await?;
    Ok((
        OcrMapping {
            viewport,
            dimensions: [pixels.width, pixels.height],
            scale,
        },
        image,
        pixels,
    ))
}

pub(super) async fn revalidate(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
    bound: &Bound,
    action: &ElementAction,
) -> Result<(), AdapterError> {
    revalidate_with(wda, session, app, bound, action, ocr::fresh).await
}

async fn revalidate_with(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
    bound: &Bound,
    action: &ElementAction,
    recognize: impl FnOnce(&Pixels) -> Result<Vec<ocr::Text>, AdapterError>,
) -> Result<(), AdapterError> {
    if !matches!(action, ElementAction::Tap { .. }) {
        return Err(AdapterError);
    }
    let binding = bound.ocr.as_ref().ok_or(AdapterError)?;
    let (mapping, _, pixels) = evidence(wda, session, app).await?;
    if binding.mapping != mapping {
        return Err(AdapterError);
    }
    let fresh = recognize(&pixels)?;
    if !binding.matches(mapping, &fresh) {
        return Err(AdapterError);
    }
    native_text_contradiction_absent(
        wda,
        session,
        binding,
        ocr::matching_text(&fresh, &binding.text).ok_or(AdapterError)?,
    )
    .await
}

pub(super) async fn dispatch(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
    bound: &Bound,
    action: &ElementAction,
) -> Result<TransportReceipt, AdapterError> {
    if !matches!(action, ElementAction::Tap { .. }) || bound.ocr.is_none() {
        return Err(AdapterError);
    }
    app_matches(wda, session, app).await?;
    let [x, y, w, h] = bound.public.rect;
    let mut receipt = WdaActions::tap_on(
        wda,
        session,
        handkite_core::protocol::Point {
            x: x + w / 2.,
            y: y + h / 2.,
        },
    )
    .await?;
    receipt.action_kind = action.kind().into();
    Ok(receipt)
}

pub(super) async fn readback(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
    bound: &Bound,
) -> Result<(ScreenImage, bool), AdapterError> {
    let (mapping, image, pixels) = evidence(wda, session, app).await?;
    let binding = bound.ocr.as_ref().ok_or(AdapterError)?;
    if binding.mapping != mapping {
        return Err(AdapterError);
    }
    let expected = &binding.text;
    let changed = pixels
        .region(mapping.viewport, &bound.attrs, &mut 0)
        .ok()
        .as_ref()
        != bound.region.as_ref();
    // Require a uniquely new text location, such as the selected district now
    // appearing in the address field. Absence or uncertain OCR is not an effect.
    let fresh = ocr::fresh(&pixels)?;
    if !ocr::safe_context(&fresh) {
        return Err(AdapterError);
    }
    let verified = changed
        && ocr::moved_text(
            bound.ocr_before.as_deref().ok_or(AdapterError)?,
            &fresh,
            expected,
        );
    Ok((image, verified))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ContextReply(u8);
    impl WdaRequest for ContextReply {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            assert_eq!(method, "POST");
            assert_eq!(path, "/session/session/elements");
            let predicate: Value = serde_json::from_slice(body).unwrap();
            let text = predicate["value"].as_str().unwrap();
            assert!(!text.contains("visible"));
            assert!(!text.contains("enabled"));
            for required in [
                "XCUIElementTypeAlert",
                "XCUIElementTypeSheet",
                "XCUIElementTypeSecureTextField",
                "proud-lock",
                "passcode-view",
                "call-to-action-label",
                "Enter Passcode",
            ] {
                assert!(text.contains(required));
            }
            let result = match self.0 {
                0 => json!([]),
                1 => json!([{"ELEMENT":"unsafe"}]),
                2 => Value::Null,
                _ => json!({}),
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":result})).unwrap(),
            })
        }
    }
    #[tokio::test]
    async fn context_requires_an_empty_complete_unfiltered_native_query() {
        assert!(
            unsafe_context_absent(&ContextReply(0), "session")
                .await
                .is_ok()
        );
        for mode in 1..=3 {
            assert!(
                unsafe_context_absent(&ContextReply(mode), "session")
                    .await
                    .is_err()
            );
        }
    }
    #[test]
    fn fresh_native_window_must_have_zero_origin_and_valid_dimensions() {
        assert_eq!(
            window_viewport(&json!({"x":0,"y":0,"width":393,"height":852})).unwrap(),
            [0., 0., 393., 852.]
        );
        for bad in [
            json!({"x":1,"y":0,"width":393,"height":852}),
            json!({"x":0,"y":1,"width":393,"height":852}),
            json!({"x":0,"y":0,"width":0,"height":852}),
            json!({"x":0,"y":0,"width":393,"height":10001}),
            json!({"width":393,"height":852}),
        ] {
            assert!(window_viewport(&bad).is_err());
        }
    }
    fn mapping() -> OcrMapping {
        OcrMapping {
            viewport: [0., 0., 200., 400.],
            dimensions: [200, 400],
            scale: 1.,
        }
    }

    fn row() -> ocr::Text {
        ocr::Text {
            text: "District".into(),
            rect: [20., 20., 80., 20.],
        }
    }
    fn xml() -> &'static str {
        r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="200" height="400"><XCUIElementTypeStaticText label="District" x="0" y="0" width="100" height="40"/></XCUIElementTypeApplication></AppiumAUT>"#
    }

    fn pixels() -> Pixels {
        Pixels {
            width: 200,
            height: 400,
            rgba: vec![255; 200 * 400 * 4],
        }
    }

    #[test]
    fn both_frames_require_unique_unchanged_text_in_the_original_region() {
        let pixels = pixels();
        let mut changed = row();
        changed.text.push('!');
        let mut drifted = row();
        drifted.rect[0] += 3.01;
        for invalid in [vec![], vec![changed], vec![row(), row()], vec![drifted]] {
            for rows in [
                [invalid.clone(), vec![row()]],
                [vec![row()], invalid.clone()],
            ] {
                let mut entries = Vec::new();
                merge_rows(
                    "fixture",
                    xml(),
                    mapping(),
                    &pixels,
                    &pixels,
                    rows,
                    &mut entries,
                )
                .unwrap();
                assert!(entries.is_empty());
            }
        }
        let mut rounded = row();
        rounded.rect[0] += 3.;
        let mut entries = Vec::new();
        merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &pixels,
            [vec![rounded], vec![row()]],
            &mut entries,
        )
        .unwrap();
        let binding = entries[0].ocr.as_ref().unwrap();
        assert!(binding.matches(mapping(), &[row()]));
        for axis in 0..4 {
            let mut moved = row();
            moved.rect[axis] += 3.01;
            assert!(!binding.matches(mapping(), &[moved]));
        }
        assert!(!binding.matches(mapping(), &[]));
        assert!(!binding.matches(mapping(), &[row(), row()]));
        assert!(!binding.matches(
            mapping(),
            &[ocr::Text {
                text: "Changed".into(),
                ..row()
            }]
        ));
    }

    #[test]
    fn unsafe_text_in_either_frame_vetoes_all_ocr_targets() {
        let pixels = pixels();
        let unsafe_row = ocr::Text {
            text: "Enter Passcode".into(),
            rect: [20., 100., 80., 20.],
        };
        for rows in [
            [vec![row(), unsafe_row.clone()], vec![row()]],
            [vec![row()], vec![row(), unsafe_row.clone()]],
        ] {
            let mut entries = Vec::new();
            assert!(
                merge_rows(
                    "fixture",
                    xml(),
                    mapping(),
                    &pixels,
                    &pixels,
                    rows,
                    &mut entries,
                )
                .is_err()
            );
            assert!(entries.is_empty());
        }
        let binding = OcrBinding {
            text: row(),
            mapping: mapping(),
        };
        assert!(!binding.matches(mapping(), &[row(), unsafe_row]));
    }

    #[test]
    fn tolerated_drift_must_still_contain_the_original_tap_point() {
        let original = ocr::Text {
            rect: [20., 20., 4., 4.],
            ..row()
        };
        let binding = OcrBinding {
            text: original.clone(),
            mapping: mapping(),
        };
        assert!(binding.matches(mapping(), &[original.clone()]));
        for axis in 0..2 {
            for shift in [-3., 3.] {
                let mut moved = original.clone();
                moved.rect[axis] += shift;
                assert!(ocr::same_text(&[moved.clone()], &original));
                assert!(!binding.matches(mapping(), &[moved]));
            }
            let mut narrowed = original.clone();
            narrowed.rect[axis + 2] = 1.;
            assert!(ocr::same_text(&[narrowed.clone()], &original));
            assert!(!binding.matches(mapping(), &[narrowed]));
            let mut safe_drift = original.clone();
            safe_drift.rect[axis] += 1.;
            assert!(binding.matches(mapping(), &[safe_drift]));
        }
    }

    struct MappingReply {
        mapping: OcrMapping,
        image: String,
        inputs: std::cell::Cell<usize>,
        native_rows: Value,
        truncate_native: bool,
    }

    impl MappingReply {
        fn new(mapping: OcrMapping) -> Self {
            let mut image = Vec::new();
            {
                let [width, height] = mapping.dimensions;
                let mut encoder = png::Encoder::new(&mut image, width, height);
                encoder.set_color(png::ColorType::Grayscale);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&vec![255; (width * height) as usize])
                    .unwrap();
            }
            Self {
                mapping,
                image: base64::engine::general_purpose::STANDARD.encode(image),
                inputs: std::cell::Cell::new(0),
                native_rows: json!([]),
                truncate_native: false,
            }
        }
    }

    impl WdaRequest for MappingReply {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let result = match (method, path) {
                ("GET", "/session/session/wda/activeAppInfo") => json!({"bundleId":"example.app"}),
                ("GET", "/session/session/wda/locked") => json!(false),
                ("POST", "/session/session/elements") => {
                    let query: Value = serde_json::from_slice(body).unwrap();
                    let predicate = query["value"].as_str().unwrap();
                    if predicate.contains(" AND (label CONTAINS") {
                        assert!(!predicate.contains("visible"));
                        assert!(!predicate.contains("displayed"));
                        assert!(!predicate.contains("enabled"));
                        for field in ["label", "name", "value"] {
                            assert!(predicate.contains(&format!("{field} CONTAINS \"D\"")));
                        }
                        if self.truncate_native {
                            return Ok(HttpResponse {
                                status: 200,
                                body: br#"{"sessionId":"session","value":["#.to_vec(),
                            });
                        }
                        self.native_rows.clone()
                    } else {
                        json!([])
                    }
                }
                ("GET", "/screenshot") => json!(self.image),
                ("GET", "/session/session/appium/settings") => json!({"currentDisplayId":1}),
                ("GET", "/wda/screens") => json!([{"displayId":1,"scale":self.mapping.scale}]),
                ("GET", "/session/session/window/rect") => {
                    let [x, y, width, height] = self.mapping.viewport;
                    json!({"x":x,"y":y,"width":width,"height":height})
                }
                ("POST", _) => {
                    self.inputs.set(self.inputs.get() + 1);
                    Value::Null
                }
                _ => panic!("unexpected read route"),
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":result})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn fresh_native_contradictions_and_incomplete_results_prevent_input() {
        let pixels = pixels();
        let mut entries = Vec::new();
        merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &pixels,
            [vec![row()], vec![row()]],
            &mut entries,
        )
        .unwrap();
        let bound = &entries[0];
        let action = ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        };
        let base = json!({
            "ELEMENT":"native-one", "type":"XCUIElementTypeButton",
            "label":null, "attribute/name":null, "attribute/value":null,
            "enabled":true, "displayed":true,
            "rect":{"x":0,"y":0,"width":100,"height":40}
        });
        let mut refused = Vec::new();
        for field in ["label", "attribute/name", "attribute/value"] {
            for state in ["enabled", "displayed"] {
                let mut native = base.clone();
                native[field] = json!(" D i s t r i c t ");
                native[state] = json!(false);
                refused.push(json!([native]));
            }
        }
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove("displayed");
        refused.extend([
            json!([missing]),
            json!({"error":"unknown error"}),
            Value::Null,
            json!(vec![base.clone(); 2049]),
        ]);
        for rows in refused {
            let mut fake = MappingReply::new(mapping());
            fake.native_rows = rows;
            let result = async {
                revalidate_with(&fake, "session", "example.app", bound, &action, |_| {
                    Ok(vec![row()])
                })
                .await?;
                dispatch(&fake, "session", "example.app", bound, &action).await
            }
            .await;
            assert!(result.is_err());
            assert_eq!(fake.inputs.get(), 0);
        }
        let mut fake = MappingReply::new(mapping());
        fake.truncate_native = true;
        assert!(
            revalidate_with(&fake, "session", "example.app", bound, &action, |_| Ok(
                vec![row()]
            ))
            .await
            .is_err()
        );
        assert_eq!(fake.inputs.get(), 0);

        let mut unrelated = base.clone();
        unrelated["label"] = json!("Different");
        unrelated["enabled"] = json!(false);
        let mut elsewhere = base.clone();
        elsewhere["label"] = json!("District");
        elsewhere["displayed"] = json!(false);
        elsewhere["rect"]["y"] = json!(100);
        let mut enabled = base;
        enabled["label"] = json!("District");
        for rows in [
            json!([]),
            json!([unrelated]),
            json!([elsewhere]),
            json!([enabled]),
        ] {
            let mut fake = MappingReply::new(mapping());
            fake.native_rows = rows;
            revalidate_with(&fake, "session", "example.app", bound, &action, |_| {
                Ok(vec![row()])
            })
            .await
            .unwrap();
            dispatch(&fake, "session", "example.app", bound, &action)
                .await
                .unwrap();
            assert_eq!(fake.inputs.get(), 1);
        }
    }

    #[tokio::test]
    async fn changed_original_mapping_refuses_before_ocr_or_input() {
        let pixels = pixels();
        let mut entries = Vec::new();
        merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &pixels,
            [vec![row()], vec![row()]],
            &mut entries,
        )
        .unwrap();
        let bound = &entries[0];
        let action = ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        };
        for changed in [
            OcrMapping {
                viewport: [0., 0., 199.5, 400.],
                ..mapping()
            },
            OcrMapping {
                dimensions: [201, 400],
                ..mapping()
            },
            OcrMapping {
                scale: 1.001,
                ..mapping()
            },
        ] {
            let fake = MappingReply::new(changed);
            // Each new mapping is valid in isolation; the original binding refuses it.
            assert_eq!(
                evidence(&fake, "session", "example.app").await.unwrap().0,
                changed
            );
            let result = async {
                revalidate(&fake, "session", "example.app", bound, &action).await?;
                dispatch(&fake, "session", "example.app", bound, &action).await
            }
            .await;
            assert!(result.is_err());
            assert_eq!(fake.inputs.get(), 0);
        }
    }

    #[test]
    fn raster_axes_map_independently_to_point_coordinates() {
        let scale = [1178. / 393., 2556. / 852.];
        assert_eq!(
            to_points([1178., 2556., 589., 1278.], scale),
            [393., 852., 196.5, 426.]
        );
    }

    #[test]
    fn attachment_requires_exact_text_unique_geometry_and_no_negative_native_evidence() {
        let xml = xml();
        assert_eq!(
            placement(&xml_nodes(xml).unwrap(), &row(), [1., 1.], &[]),
            Placement::Attach(2)
        );
        assert_eq!(
            placement(
                &xml_nodes(&xml.replace("District", "District extended")).unwrap(),
                &row(),
                [1., 1.],
                &[]
            ),
            Placement::Synthetic
        );
        assert_eq!(
            placement(
                &xml_nodes(&xml.replace("StaticText", "Other")).unwrap(),
                &row(),
                [1., 1.],
                &[]
            ),
            Placement::Synthetic
        );
        for attribute in ["enabled=\"false\"", "visible=\"false\""] {
            assert_eq!(
                placement(
                    &xml_nodes(&xml.replace("label=", &format!("{attribute} label="))).unwrap(),
                    &row(),
                    [1., 1.],
                    &[]
                ),
                Placement::Suppress
            );
        }
        let duplicate = xml.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeStaticText label=\"District\" x=\"0\" y=\"0\" width=\"100\" height=\"40\"/></XCUIElementTypeApplication>");
        assert_eq!(
            placement(&xml_nodes(&duplicate).unwrap(), &row(), [1., 1.], &[]),
            Placement::Suppress
        );
        let displaced = ocr::Text {
            rect: [120., 20., 80., 20.],
            ..row()
        };
        assert_eq!(
            placement(&xml_nodes(xml).unwrap(), &displaced, [1., 1.], &[]),
            Placement::Synthetic
        );
        assert!(xml_nodes(&xml.replace("StaticText", "SecureTextField")).is_err());
    }

    #[test]
    fn conflicting_native_state_cannot_fall_back_to_an_ocr_handle() {
        let xml = xml()
            .replace("StaticText", "Button")
            .replace("label=", "enabled=\"true\" label=");
        let base = json!({"type":"XCUIElementTypeButton", "label":"District", "attribute/name":null,
            "attribute/value":null, "attribute/placeholderValue":null, "attribute/focused":false,
            "enabled":true, "displayed":true, "rect":{"x":0,"y":0,"width":100,"height":40}});
        let pixels = Pixels {
            width: 200,
            height: 400,
            rgba: vec![255; 200 * 400 * 4],
        };
        for key in ["enabled", "displayed"] {
            let mut item = base.clone();
            item[key] = json!(false);
            let projected = native_visibility(&xml, &json!([item])).unwrap();
            let mut entries = Vec::new();
            merge_rows(
                "fixture",
                &projected,
                mapping(),
                &pixels,
                &pixels,
                [vec![row()], vec![row()]],
                &mut entries,
            )
            .unwrap();
            assert!(entries.is_empty(), "{key}");
        }
    }

    #[test]
    fn attached_row_preserves_xml_identity_and_uses_ocr_glyph_for_tap_only() {
        let pixels = Pixels {
            width: 200,
            height: 400,
            rgba: vec![255; 200 * 400 * 4],
        };
        let mut entries = Vec::new();
        let result = merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &pixels,
            [vec![row()], vec![row()]],
            &mut entries,
        )
        .unwrap();
        assert_eq!(xml_nodes(&result).unwrap().len(), 3);
        let node = &xml_nodes(&result).unwrap()[2];
        assert_eq!(node.get("input_source").map(String::as_str), Some("ocr"));
        assert_eq!(node.get("label").map(String::as_str), Some("District"));
        assert!(!xml().contains("visible=")); // Native admission input is never mutated.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].public.rect, [20., 20., 80., 20.]);
        assert_eq!(entries[0].public.source, "ocr");
        assert!(!entries[0].public.can_type);
        assert!(entries[0].native.is_empty());
        assert_eq!(node.get("target_id"), Some(&entries[0].public.target_id));
        let mut duplicate = Vec::new();
        merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &pixels,
            [vec![row(), row()], vec![row(), row()]],
            &mut duplicate,
        )
        .unwrap();
        assert!(duplicate.is_empty());
        let mut changed = Pixels {
            width: 200,
            height: 400,
            rgba: pixels.rgba.clone(),
        };
        changed.rgba[(15 * 200 + 15) * 4] = 0;
        let mut stable_text = Vec::new();
        merge_rows(
            "fixture",
            xml(),
            mapping(),
            &pixels,
            &changed,
            [vec![row()], vec![row()]],
            &mut stable_text,
        )
        .unwrap();
        assert_eq!(stable_text.len(), 1);
        assert!(
            stable_text[0]
                .ocr
                .as_ref()
                .unwrap()
                .matches(mapping(), &[row()])
        );
    }
}
