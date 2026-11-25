//! Opt-in, one-use WDA identity handles. Rich responses are not an atomic snapshot.
use super::focused_text::{active, app_matches, id, rich_context, value};
use super::*;
use crate::wda_ui_admission::Attributes;
use handkite_core::controller::{ElementAction, ElementTarget, PreparedTarget, PreparedText};
use handkite_core::protocol::{Observation, Revision};
use quick_xml::events::Event;
use serde_json::{Value, json};
use std::collections::BTreeSet;
#[path = "targets_budget.rs"]
mod budget;
use budget::{TargetWorkError, target_context};
#[path = "targets_delta.rs"]
mod delta;
pub(super) use delta::DeltaBaseline;
#[path = "targets_fast.rs"]
mod fast;
#[path = "targets_pending.rs"]
mod pending;
#[path = "targets_text.rs"]
mod text;
pub(super) use pending::PendingEffect;
#[path = "targets_ocr.rs"]
mod vision;

const ATTRS: &str = "type,enabled,displayed,rect,label,attribute/name,attribute/value,attribute/placeholderValue,attribute/focused,attribute/selected";
const AGE: Duration = Duration::from_secs(15);
// Cache reuse must fit handkited's five-second capture and freshness limits.
const CACHE_WINDOW_MS: u64 = 5_000;
// Reserve a two-second next-action budget inside the existing freshness limit.
// Recapture instead of publishing a cached target near its original expiry.
const ACTION_HEADROOM_MS: u64 = 2_000;
// Limits publication after recovery; an expired read still drains restoration.
const OCR_RECOVERY_PUBLICATION_BUDGET: Duration = Duration::from_secs(20);
const SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,index,traits,hittable,customActions,nativeFrame,visible";
const FULL_SOURCE: &str = "/source?format=xml&excluded_attributes=accessible,index,traits,hittable,customActions,nativeFrame";

#[derive(Clone)]
struct Bound {
    public: ElementTarget,
    native: String,
    attrs: Attributes,
    rich: Value,
    region: Option<super::targets_pixels::Region>,
    focused: Option<bool>,
    ocr: Option<vision::OcrBinding>,
    ocr_before: Option<std::sync::Arc<Vec<super::ocr::Text>>>,
    mapping: Option<fast::Mapping>,
}

#[derive(Clone)]
struct EvidenceWindow {
    window: ObservationWindow,
    started: Instant,
    finished: Instant,
}

impl EvidenceWindow {
    fn until(&self) -> Instant {
        self.started + AGE
    }

    fn reusable_at(&self, now: Instant, unix_ms: u64) -> bool {
        now < self.until()
            && now
                .checked_duration_since(self.finished)
                .is_some_and(|age| age < Duration::from_millis(CACHE_WINDOW_MS))
            && self
                .window
                .duration_ms()
                .is_ok_and(|duration| duration <= CACHE_WINDOW_MS)
            && unix_ms
                .checked_sub(self.window.finished_unix_ms)
                .is_some_and(|age| age < CACHE_WINDOW_MS)
    }

    fn reusable(&self) -> Result<bool, AdapterError> {
        Ok(self.reusable_at(Instant::now(), unix_ms()?))
    }

    fn actionable_at(&self, now: Instant, unix_ms: u64) -> bool {
        let age_limit = CACHE_WINDOW_MS - ACTION_HEADROOM_MS;
        self.reusable_at(now, unix_ms)
            && now
                .checked_duration_since(self.finished)
                .is_some_and(|age| age < Duration::from_millis(age_limit))
            && unix_ms
                .checked_sub(self.window.finished_unix_ms)
                .is_some_and(|age| age < age_limit)
    }

    fn actionable(&self) -> Result<bool, AdapterError> {
        Ok(self.actionable_at(Instant::now(), unix_ms()?))
    }
}

struct Captured {
    xml: String,
    items: Value,
    before: ScreenImage,
    after: ScreenImage,
    app: String,
    focused: Option<String>,
    evidence: EvidenceWindow,
    scale: f64,
    display_id: Option<u64>,
    reused: bool,
}

enum CapturePlan {
    Snapshot(Captured),
    Fresh(CaptureStart),
}

struct CaptureStart {
    started: Instant,
    started_unix_ms: u64,
    display_id: Option<u64>,
    app: String,
    before: ScreenImage,
}

#[derive(Clone)]
pub(super) struct Snapshot {
    key: cache::Key,
    xml: String,
    items: Value,
    app: String,
    image: ScreenImage,
    evidence: EvidenceWindow,
    scale: f64,
    display_id: Option<u64>,
    focused: Option<String>,
    bracket_equal: bool,
    cost: super::outline::Cost,
}

pub(super) struct Targets {
    key: cache::Key,
    revision: Option<Revision>,
    app: String,
    until: Instant,
    entries: Vec<Bound>,
}

impl Targets {
    fn context_matches(
        &self,
        key: Option<&cache::Key>,
        revision: Revision,
        app: Option<&str>,
        now: Instant,
    ) -> bool {
        self.revision == Some(revision)
            && now < self.until
            && key == Some(&self.key)
            && app == Some(&self.app)
    }
}

#[derive(Clone)]
pub(super) struct Input {
    token: u64,
    key: cache::Key,
    app: String,
    bound: Bound,
    action: ElementAction,
    until: Instant,
    text_basis: Option<Snapshot>,
}

impl Input {
    fn prepared(&self, device: &HardwareId, baseline: &Observation) -> PreparedTarget {
        PreparedTarget {
            evidence: PreparedText {
                connected_usb: device.clone(),
                wda_target: device.clone(),
                window: baseline.window.clone(),
                state: baseline.state.clone(),
                token: self.token,
                valid_until: self.until,
            },
        }
    }
}

pub(super) fn xml_nodes(xml: &str) -> Result<Vec<Attributes>, AdapterError> {
    if !crate::wda_ui_admission::safe_context(xml) {
        return Err(AdapterError);
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut nodes = Vec::new();
    loop {
        match reader.read_event().map_err(|_| AdapterError)? {
            Event::Start(node) | Event::Empty(node) => {
                let mut attrs = Attributes::new();
                for attr in node.attributes() {
                    let attr = attr.map_err(|_| AdapterError)?;
                    let key = std::str::from_utf8(attr.key.as_ref())
                        .map_err(|_| AdapterError)?
                        .to_owned();
                    if attrs
                        .insert(
                            key,
                            attr.decode_and_unescape_value(reader.decoder())
                                .map_err(|_| AdapterError)?
                                .into_owned(),
                        )
                        .is_some()
                    {
                        return Err(AdapterError);
                    }
                }
                let kind = std::str::from_utf8(node.name().as_ref())
                    .map_err(|_| AdapterError)?
                    .to_owned();
                if attrs.get("type").is_some_and(|value| value != &kind) {
                    return Err(AdapterError);
                }
                attrs.insert("type".into(), kind);
                nodes.push(attrs);
                if nodes.len() > 20_000 {
                    return Err(AdapterError);
                }
            }
            Event::Eof => return Ok(nodes),
            _ => {}
        }
    }
}

fn descriptor_matches(item: &Value, attrs: &Attributes) -> bool {
    item["type"]
        .as_str()
        .is_some_and(|kind| attrs.get("type").map(String::as_str) == Some(kind))
        && match item.get("label") {
            Some(Value::String(label)) => {
                attrs.get("label").map(String::as_str).unwrap_or("") == label
            }
            Some(Value::Null) => attrs.get("label").is_none_or(String::is_empty),
            _ => false,
        }
        && ["name", "value", "placeholderValue"].iter().all(|key| {
            match item.get(format!("attribute/{key}")) {
                Some(Value::String(v)) => attrs.get(*key).map(String::as_str).unwrap_or("") == v,
                Some(Value::Null) => attrs.get(*key).is_none_or(String::is_empty),
                _ => false,
            }
        })
        && ["x", "y", "width", "height"].iter().all(|key| {
            let number = item["rect"][*key].as_f64();
            number.is_some_and(|v| {
                v.is_finite() && attrs.get(*key).and_then(|v| v.parse::<f64>().ok()) == Some(v)
            })
        })
}

fn identity_matches(item: &Value, attrs: &Attributes) -> bool {
    descriptor_matches(item, attrs)
        && item["enabled"].as_bool().is_some_and(|enabled| {
            attrs.get("enabled").map(String::as_str) == Some(if enabled { "true" } else { "false" })
        })
        && item.get("attribute/focused").is_some_and(|v| {
            let expected = attrs.get("focused").map(String::as_str).unwrap_or("false");
            v.as_bool().map(|b| if b { "true" } else { "false" }) == Some(expected)
                || v.as_str() == Some(expected)
        })
}

fn matches_xml(item: &Value, attrs: &Attributes) -> bool {
    identity_matches(item, attrs)
        && item["enabled"] == true
        && item["displayed"] == true
        && attrs.get("enabled").map(String::as_str) == Some("true")
        && attrs.get("visible").map(String::as_str) == Some("true")
}

fn native_visibility(xml: &str, items: &Value) -> Result<String, AdapterError> {
    let nodes = xml_nodes(xml)?;
    let rows = items.as_array().ok_or(AdapterError)?;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut writer = quick_xml::Writer::new(Vec::new());
    let mut index = 0;
    loop {
        let event = reader.read_event().map_err(|_| AdapterError)?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(node) | Event::Empty(node) => {
                let attrs = nodes.get(index).ok_or(AdapterError)?;
                index += 1;
                let matches: Vec<_> = rows
                    .iter()
                    .filter(|row| descriptor_matches(row, attrs))
                    .collect();
                let mut out = node.to_owned();
                if !matches.is_empty() {
                    out.clear_attributes();
                    for a in node.attributes() {
                        let a = a.map_err(|_| AdapterError)?;
                        if a.key.as_ref() != b"visible" {
                            out.push_attribute(a);
                        }
                    }
                    out.push_attribute((
                        "visible",
                        if matches.len() == 1
                            && identity_matches(matches[0], attrs)
                            && matches[0]["displayed"] == true
                            && attrs.get("visible").map(String::as_str) != Some("false")
                        {
                            "true"
                        } else {
                            "false"
                        },
                    ));
                }
                writer
                    .write_event(if empty {
                        Event::Empty(out)
                    } else {
                        Event::Start(out)
                    })
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

fn editable_or_keyboard(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(
            "XCUIElementTypeTextField"
                | "XCUIElementTypeTextView"
                | "XCUIElementTypeSearchField"
                | "XCUIElementTypeKeyboard"
        )
    )
}
fn complete_text_context(xml: &str, items: &Value) -> bool {
    let Ok(nodes) = xml_nodes(xml) else {
        return false;
    };
    let Some(rows) = items.as_array() else {
        return false;
    };
    let fields: Vec<_> = nodes
        .iter()
        .filter(|a| editable_or_keyboard(a.get("type").map(String::as_str)))
        .collect();
    let native: Vec<_> = rows
        .iter()
        .filter(|row| editable_or_keyboard(row["type"].as_str()))
        .collect();
    fields.len() == native.len()
        && fields.iter().all(|a| {
            native
                .iter()
                .filter(|row| {
                    identity_matches(row, a)
                        && row["displayed"].as_bool().is_some_and(|displayed| {
                            a.get("visible").map(String::as_str)
                                == Some(if displayed { "true" } else { "false" })
                        })
                })
                .count()
                == 1
        })
        && native
            .iter()
            .all(|row| fields.iter().filter(|a| identity_matches(row, a)).count() == 1)
}

fn candidates(xml: &str, items: &Value) -> Result<Vec<Bound>, AdapterError> {
    let nodes = xml_nodes(xml)?;
    let windows: Vec<_> = nodes
        .iter()
        .filter(|a| a.get("type").map(String::as_str) == Some("XCUIElementTypeApplication"))
        .collect();
    if windows.len() != 1 {
        return Err(AdapterError);
    }
    let number = |a: &Attributes, key: &str| {
        a.get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or(AdapterError)
    };
    let window = windows[0];
    let (wx, wy, ww, wh) = (
        number(window, "x")?,
        number(window, "y")?,
        number(window, "width")?,
        number(window, "height")?,
    );
    let items = items
        .as_array()
        .filter(|v| v.len() <= 2048)
        .ok_or(AdapterError)?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut matched_nodes = BTreeSet::new();
    for item in items {
        let native = id(item)?;
        if !seen.insert(native.clone()) {
            return Err(AdapterError);
        }
        let kind = item["type"].as_str().ok_or(AdapterError)?;
        for field in [
            "label",
            "attribute/name",
            "attribute/value",
            "attribute/placeholderValue",
        ] {
            if !item
                .get(field)
                .is_some_and(|v| v.is_string() || v.is_null())
            {
                return Err(AdapterError);
            }
        }
        if !item
            .get("attribute/focused")
            .is_some_and(|v| v.is_boolean() || matches!(v.as_str(), Some("true" | "false")))
            || !item["rect"].is_object()
        {
            return Err(AdapterError);
        }
        if !matches!(
            kind,
            "XCUIElementTypeButton"
                | "XCUIElementTypeMenuItem"
                | "XCUIElementTypeCell"
                | "XCUIElementTypeStaticText"
                | "XCUIElementTypeTextField"
                | "XCUIElementTypeTextView"
                | "XCUIElementTypeSearchField"
                | "XCUIElementTypeLink"
                | "XCUIElementTypeImage"
                | "XCUIElementTypePickerWheel"
        ) {
            continue;
        }
        if item["enabled"].as_bool() != Some(true) || item["displayed"].as_bool() != Some(true) {
            continue;
        }
        if item.get("attribute/hittable").is_some_and(|v| v != true) {
            continue;
        }
        let matched: Vec<_> = nodes.iter().filter(|a| matches_xml(item, a)).collect();
        if matched.len() != 1 {
            continue;
        }
        let attrs = matched[0];
        if !matched_nodes.insert(serde_json::to_string(attrs).map_err(|_| AdapterError)?) {
            return Err(AdapterError);
        }
        let (x, y, w, h) = (
            number(attrs, "x")?,
            number(attrs, "y")?,
            number(attrs, "width")?,
            number(attrs, "height")?,
        );
        if w <= 0.0 || h <= 0.0 || x < wx || y < wy || x + w > wx + ww || y + h > wy + wh {
            continue;
        }
        let mut random = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
            .map_err(|_| AdapterError)?;
        let target_id = format!(
            "t_{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let can_type = matches!(
            kind,
            "XCUIElementTypeTextField" | "XCUIElementTypeTextView" | "XCUIElementTypeSearchField"
        ) && attrs.get("focused").map(String::as_str) == Some("true");
        out.push(Bound {
            public: ElementTarget {
                source: "xml".into(),
                target_id,
                kind: kind.into(),
                label: attrs.get("label").cloned().unwrap_or_default(),
                can_type,
                text_only: false,
                rect: [x, y, w, h],
                value: attrs
                    .get("value")
                    .map(|value| value.chars().take(256).collect()),
                value_truncated: attrs
                    .get("value")
                    .is_some_and(|value| value.chars().count() > 256),
            },
            native,
            attrs: attrs.clone(),
            rich: item.clone(),
            region: None,
            focused: None,
            ocr: None,
            ocr_before: None,
            mapping: None,
        });
    }
    Ok(out)
}

async fn query(wda: &impl WdaRequest, session: &str) -> Result<Value, AdapterError> {
    query_predicate(wda, session, "type IN {'XCUIElementTypeButton','XCUIElementTypeMenuItem','XCUIElementTypeCell','XCUIElementTypeStaticText','XCUIElementTypeTextField','XCUIElementTypeTextView','XCUIElementTypeSearchField','XCUIElementTypeLink','XCUIElementTypeImage','XCUIElementTypePickerWheel','XCUIElementTypeKeyboard'}").await
}

async fn query_predicate(
    wda: &impl WdaRequest,
    session: &str,
    predicate: &str,
) -> Result<Value, AdapterError> {
    value(
        wda,
        session,
        "POST",
        &format!("/session/{session}/elements"),
        &serde_json::to_vec(&json!({"using":"predicate string", "value":predicate}))
            .map_err(|_| AdapterError)?,
    )
    .await
}

fn input_predicate(bound: &Bound, action: &ElementAction) -> Result<String, AdapterError> {
    if matches!(action, ElementAction::Text { .. }) {
        // Include every field and keyboard needed by complete_text_context.
        return Ok("type IN {'XCUIElementTypeTextField','XCUIElementTypeTextView','XCUIElementTypeSearchField','XCUIElementTypeKeyboard'}".into());
    }
    // Narrow the lookup, never the comparison: the returned native ID and all
    // attributes still have to match the bound target, XML and guarded pixels.
    let kind = serde_json::to_string(&bound.public.kind).map_err(|_| AdapterError)?;
    let label = match bound.rich.get("label").ok_or(AdapterError)? {
        Value::String(label) if !label.is_empty() => {
            serde_json::to_string(label).map_err(|_| AdapterError)?
        }
        // WDA may transfer an empty label to null. Keep the old capability by
        // narrowing by type only, with the same exact identity comparison.
        Value::Null | Value::String(_) => return Ok(format!("type == {kind}")),
        _ => return Err(AdapterError),
    };
    Ok(format!("type == {kind} AND label == {label}"))
}

fn same_target(expected: &Bound, current: &[Bound]) -> bool {
    current
        .iter()
        .filter(|candidate| candidate.native == expected.native)
        .count()
        == 1
        && current.iter().any(|candidate| {
            candidate.native == expected.native
                && candidate.attrs == expected.attrs
                && candidate.rich == expected.rich
        })
}

pub(super) async fn screenshot(wda: &impl WdaRequest) -> Result<ScreenImage, AdapterError> {
    let image = response_value(
        wda.request("GET", "/screenshot", b"", Duration::from_secs(10))
            .await
            .map_err(|_| AdapterError)?,
    )?;
    let encoded = image
        .as_str()
        .filter(|s| s.len() <= 44 * 1024 * 1024)
        .ok_or(AdapterError)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| AdapterError)?;
    Ok(ScreenImage {
        format: ImageFormat::Png,
        bytes,
    })
}

#[derive(Debug)]
enum CaptureFailure {
    HomeScreen,
    Adapter,
}
impl From<AdapterError> for CaptureFailure {
    fn from(_: AdapterError) -> Self {
        Self::Adapter
    }
}
impl From<CaptureFailure> for AdapterError {
    fn from(_: CaptureFailure) -> Self {
        AdapterError
    }
}

async fn captured(
    wda: &impl WdaRequest,
    session: &str,
    cached: Option<&Snapshot>,
) -> Result<Captured, CaptureFailure> {
    captured_for(wda, session, cached, None, true).await
}

async fn captured_for(
    wda: &impl WdaRequest,
    session: &str,
    cached: Option<&Snapshot>,
    predicate: Option<&str>,
    capture_focus: bool,
) -> Result<Captured, CaptureFailure> {
    if cached.is_some() && predicate.is_some() {
        return Err(CaptureFailure::Adapter);
    }
    match capture_plan(wda, session, cached, capture_focus).await? {
        CapturePlan::Snapshot(captured) => Ok(captured),
        CapturePlan::Fresh(start) => {
            captured_fresh(wda, session, start, predicate, capture_focus).await
        }
    }
}

async fn capture_plan(
    wda: &impl WdaRequest,
    session: &str,
    cached: Option<&Snapshot>,
    capture_focus: bool,
) -> Result<CapturePlan, CaptureFailure> {
    let started = Instant::now();
    let started_unix_ms = unix_ms()?;
    let display_id = super::display::selection(wda, session).await?;
    let app = value(
        wda,
        session,
        "GET",
        &format!("/session/{session}/wda/activeAppInfo"),
        b"",
    )
    .await?["bundleId"]
        .as_str()
        .filter(|app| !app.is_empty())
        .ok_or(AdapterError)?
        .to_owned();
    if app == "com.apple.springboard" {
        return Err(CaptureFailure::HomeScreen);
    }
    app_matches(wda, session, &app).await?;
    let mut before = {
        let _span = crate::timing::Span::new("target_image_before");
        screenshot(wda).await?
    };
    let now_ms = unix_ms()?;
    let reused = cached.filter(|cache| {
        cache.bracket_equal
            && cache.cost.within_visibility_budget()
            && cache.display_id == display_id
            && cache.app == app
            && cache.evidence.actionable_at(Instant::now(), now_ms)
            && cache.image.bytes == before.bytes
    });
    if let Some(cache) = reused {
        let focused = if capture_focus {
            active(wda, session).await.ok()
        } else {
            None
        };
        let after = screenshot(wda).await?;
        app_matches(wda, session, &app).await?;
        super::display::verify_selection(wda, session, display_id).await?;
        if before.bytes == after.bytes && cache.evidence.actionable()? {
            crate::timing::outcome("target_snapshot_reuse", true);
            return Ok(CapturePlan::Snapshot(Captured {
                xml: cache.xml.clone(),
                items: cache.items.clone(),
                before,
                after,
                app,
                focused,
                evidence: cache.evidence.clone(),
                scale: cache.scale,
                display_id,
                reused: true,
            }));
        }
        // The rejected cache frame is the new bracket start. Comparing the
        // first and third frames could hide an observed A-to-B-to-A change.
        before = after;
    }
    crate::timing::outcome("target_snapshot_reuse", false);
    Ok(CapturePlan::Fresh(CaptureStart {
        started,
        started_unix_ms,
        display_id,
        app,
        before,
    }))
}

async fn captured_fresh(
    wda: &impl WdaRequest,
    session: &str,
    start: CaptureStart,
    predicate: Option<&str>,
    capture_focus: bool,
) -> Result<Captured, CaptureFailure> {
    let CaptureStart {
        started,
        started_unix_ms,
        display_id,
        app,
        before,
    } = start;
    // All four reads share the established response profile and remain inside
    // the same screenshot/app/lock brackets. No input runs in this interval.
    let window_path = format!("/session/{session}/window/size");
    let (xml, items, screen, window) = tokio::join!(
        async {
            let _span = crate::timing::Span::new("target_source");
            response_value(
                wda.request("GET", SOURCE, b"", Duration::from_secs(10))
                    .await
                    .map_err(|_| AdapterError)?,
            )?
            .as_str()
            .map(str::to_owned)
            .ok_or(AdapterError)
        },
        async {
            let _span = crate::timing::Span::new("target_native_inventory");
            match predicate {
                Some(predicate) => query_predicate(wda, session, predicate).await,
                None => query(wda, session).await,
            }
        },
        super::display::screen_scale(wda, display_id),
        value(wda, session, "GET", &window_path, b"")
    );
    let (xml, items, screen, window) = (xml?, items?, screen?, window?);
    let focused = if capture_focus {
        active(wda, session).await.ok()
    } else {
        None
    };
    let after = screenshot(wda).await?;
    let viewport = super::targets_pixels::viewport(&xml)?;
    let scale = screen;
    let window = [
        window["width"].as_f64().ok_or(AdapterError)?,
        window["height"].as_f64().ok_or(AdapterError)?,
    ];
    super::targets_pixels::Pixels::decode(&before.bytes)?
        .validate_mapping(viewport, scale, window)?;
    super::targets_pixels::Pixels::decode(&after.bytes)?
        .validate_mapping(viewport, scale, window)?;

    app_matches(wda, session, &app).await?;
    super::display::verify_selection(wda, session, display_id).await?;
    Ok(Captured {
        xml,
        items,
        before,
        after,
        app,
        focused,
        evidence: EvidenceWindow {
            window: ObservationWindow {
                started_unix_ms,
                finished_unix_ms: unix_ms()?,
            },
            started,
            finished: Instant::now(),
        },
        scale,
        display_id,
        reused: false,
    })
}

fn bind_regions(
    xml: &str,
    entries: &mut Vec<Bound>,
    before: &ScreenImage,
    after: &ScreenImage,
) -> Result<(), AdapterError> {
    let viewport = super::targets_pixels::viewport(xml)?;
    let before = super::targets_pixels::Pixels::decode(&before.bytes)?;
    let after = super::targets_pixels::Pixels::decode(&after.bytes)?;
    let mut before_budget = 0;
    let mut after_budget = 0;
    for entry in entries.iter_mut() {
        before.region_bounds(viewport, &entry.attrs)?;
        after.region_bounds(viewport, &entry.attrs)?;
        let first = before.region(viewport, &entry.attrs, &mut before_budget);
        let last = after.region(viewport, &entry.attrs, &mut after_budget);
        entry.region = match (first, last) {
            (Ok(first), Ok(last)) => {
                entry.public.text_only = entry.focused == Some(true)
                    && entry.public.can_type
                    && matches!(
                        entry.public.kind.as_str(),
                        "XCUIElementTypeTextField"
                            | "XCUIElementTypeTextView"
                            | "XCUIElementTypeSearchField"
                    );
                (first == last || entry.public.text_only).then_some(last)
            }
            _ => None,
        };
    }
    entries.retain(|entry| entry.region.is_some());
    Ok(())
}

async fn revalidate(
    wda: &impl WdaRequest,
    key: &cache::Key,
    app: &str,
    bound: &Bound,
    action: &ElementAction,
) -> Result<(), AdapterError> {
    if bound.public.text_only && !matches!(action, ElementAction::Text { .. }) {
        return Err(AdapterError);
    }
    if bound.ocr.is_some() {
        return vision::revalidate(wda, &key.session, app, bound, action).await;
    }
    let predicate = input_predicate(bound, action)?;
    let Captured {
        xml,
        items,
        before,
        after,
        app: current_app,
        ..
    } = captured_for(
        wda,
        &key.session,
        None,
        Some(&predicate),
        matches!(action, ElementAction::Text { .. }),
    )
    .await?;
    if current_app != app {
        return Err(AdapterError);
    }
    let xml = native_visibility(&xml, &items)?;
    let mut current = candidates(&xml, &items)?;
    if !same_target(bound, &current) {
        return Err(AdapterError);
    }
    if value(
        wda,
        &key.session,
        "GET",
        &format!(
            "/session/{}/element/{}/attribute/hittable",
            key.session, bound.native
        ),
        b"",
    )
    .await?
        != true
    {
        return Err(AdapterError);
    }
    // Hash only the selected region; unrelated animation cannot widen admission.
    current.retain(|candidate| candidate.native == bound.native);
    if bound.public.text_only {
        if bound.focused != Some(true)
            || !bound.public.can_type
            || active(wda, &key.session).await? != bound.native
        {
            return Err(AdapterError);
        }
        let viewport = super::targets_pixels::viewport(&xml)?;
        super::targets_pixels::Pixels::decode(&before.bytes)?.region(
            viewport,
            &bound.attrs,
            &mut 0,
        )?;
        super::targets_pixels::Pixels::decode(&after.bytes)?.region(
            viewport,
            &bound.attrs,
            &mut 0,
        )?;
    } else {
        bind_regions(&xml, &mut current, &before, &after)?;
        if current.len() != 1 || current[0].region != bound.region || bound.region.is_none() {
            return Err(AdapterError);
        }
    }

    if let ElementAction::Text { .. } = action {
        if !complete_text_context(&xml, &items) {
            return Err(AdapterError);
        }
        let full;
        let snapshot = if let Some(snapshot) =
            crate::wda_ui_admission::selected_text_snapshot(&xml, &bound.attrs)
                .or_else(|| crate::wda_ui_admission::current_text_snapshot(&xml))
        {
            snapshot
        } else {
            // Emoji search needs native visibility for its separate container.
            // Unknown ancestor visibility is never inferred from visible children.
            full = response_value(
                wda.request("GET", FULL_SOURCE, b"", Duration::from_secs(10))
                    .await
                    .map_err(|_| AdapterError)?,
            )?
            .as_str()
            .ok_or(AdapterError)?
            .to_owned();
            if !complete_text_context(&full, &items) {
                return Err(AdapterError);
            }
            crate::wda_ui_admission::selected_text_snapshot(&full, &bound.attrs)
                .or_else(|| crate::wda_ui_admission::current_text_snapshot(&full))
                .ok_or(AdapterError)?
        };
        if snapshot.field != bound.attrs || active(wda, &key.session).await? != bound.native {
            return Err(AdapterError);
        }
    }
    app_matches(wda, &key.session, app).await
}

async fn revalidate_input(wda: &impl WdaRequest, input: &Input) -> Result<(), AdapterError> {
    match &input.text_basis {
        Some(snapshot) => text::revalidate(wda, input, snapshot).await,
        None => revalidate(wda, &input.key, &input.app, &input.bound, &input.action).await,
    }
}

async fn dispatch_target(
    wda: &impl WdaRequest,
    actions: &mut WdaActions,
    input: &Input,
    deadline: Instant,
) -> Result<TransportReceipt, AdapterError> {
    before_input_deadline(deadline, async {
        actions
            .validate_existing_diagnostic(wda)
            .await
            .map_err(|_| AdapterError)
    })
    .await?;
    // UsbWda enforces this deadline through the last request write. Once sent,
    // preserve its bounded response window instead of cancelling a valid ack.
    if Instant::now() >= deadline {
        return Err(AdapterError);
    }
    if input.bound.ocr.is_some() {
        vision::dispatch(
            wda,
            &input.key.session,
            &input.app,
            &input.bound,
            &input.action,
        )
        .await
    } else {
        dispatch_native(
            wda,
            &input.key.session,
            &input.app,
            &input.bound.native,
            &input.action,
        )
        .await
    }
}

pub(crate) async fn dispatch_native(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
    native: &str,
    action: &ElementAction,
) -> Result<TransportReceipt, AdapterError> {
    app_matches(wda, session, app).await?;
    if matches!(action, ElementAction::Text { .. }) && active(wda, session).await? != native {
        return Err(AdapterError);
    }
    if let ElementAction::Text { value, .. } = action {
        let _keys = crate::timing::Span::new("target_keys");
        let mut receipt = WdaActions::execute_on_diagnostic(
            wda,
            &Action::TextCurrent {
                value: value.clone(),
            },
            session,
            // Element text keeps its qualified unshaped request.
            None,
        )
        .await
        .map_err(|_| AdapterError)?;
        receipt.action_kind = action.kind().into();
        return Ok(receipt);
    }
    let path = format!("/session/{session}/element/{native}/click");
    let body = json!({});
    value(
        wda,
        session,
        "POST",
        &path,
        &serde_json::to_vec(&body).map_err(|_| AdapterError)?,
    )
    .await?;
    Ok(TransportReceipt {
        accepted: Some(true),
        action_kind: action.kind().into(),
    })
}

fn tap_effect(
    bound: &Bound,
    xml: &str,
    items: &Value,
    focused: Option<&str>,
) -> Result<bool, AdapterError> {
    let nodes = xml_nodes(xml)?;
    let rows = items
        .as_array()
        .filter(|rows| rows.len() <= 2048)
        .ok_or(AdapterError)?;
    let mut ids = BTreeSet::new();
    let mut found = None;
    for row in rows {
        let native = id(row)?;
        let text = |key: &str| row.get(key).is_some_and(|v| v.is_string() || v.is_null());
        let boolean = |key: &str| {
            row.get(key)
                .is_some_and(|v| v.is_boolean() || matches!(v.as_str(), Some("true" | "false")))
        };
        if !row["type"].is_string()
            || !text("label")
            || !text("attribute/name")
            || !text("attribute/value")
            || !boolean("attribute/selected")
            || !boolean("attribute/focused")
            || !row["enabled"].is_boolean()
            || !row["displayed"].is_boolean()
            || !["x", "y", "width", "height"]
                .iter()
                .all(|k| row["rect"][*k].as_f64().is_some_and(f64::is_finite))
        {
            return Err(AdapterError);
        }
        let rect = &row["rect"];
        if rect["x"].as_f64().unwrap() < 0.0
            || rect["y"].as_f64().unwrap() < 0.0
            || rect["width"].as_f64().unwrap() <= 0.0
            || rect["height"].as_f64().unwrap() <= 0.0
        {
            return Err(AdapterError);
        }
        if !ids.insert(native.clone()) {
            return Err(AdapterError);
        }
        if native == bound.native {
            found = Some(row);
        }
    }
    let Some(current) = found else {
        // An ID remap with the same object attributes is not disappearance.
        if nodes.iter().any(|attrs| {
            ["type", "name", "label", "x", "y", "width", "height"]
                .iter()
                .all(|key| attrs.get(*key) == bound.attrs.get(*key))
        }) {
            return Ok(false);
        }
        return Ok(!rows.iter().any(|row| {
            ["type", "label", "attribute/name", "rect"]
                .iter()
                .all(|field| row.get(*field) == bound.rich.get(*field))
        }));
    };
    if ["type", "label", "attribute/name"]
        .iter()
        .any(|field| current.get(*field) != bound.rich.get(*field))
    {
        return Ok(false);
    }
    if bound.focused == Some(false)
        && focused == Some(&bound.native)
        && matches!(
            bound.public.kind.as_str(),
            "XCUIElementTypeTextField" | "XCUIElementTypeTextView" | "XCUIElementTypeSearchField"
        )
    {
        return Ok(true);
    }
    if current.get("rect") != bound.rich.get("rect") {
        return Ok(false);
    }
    for field in ["enabled", "displayed", "attribute/selected"] {
        let boolean = |value: &Value| {
            value.as_bool().or_else(|| match value.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
        };
        let before = bound
            .rich
            .get(field)
            .and_then(boolean)
            .ok_or(AdapterError)?;
        let after = current.get(field).and_then(boolean).ok_or(AdapterError)?;
        if before != after {
            return Ok(true);
        }
    }
    let before = bound.rich.get("attribute/value").ok_or(AdapterError)?;
    let after = current.get("attribute/value").ok_or(AdapterError)?;
    if before != after {
        return Ok(true);
    }

    Ok(false)
}

type TargetCapture = (RawObservation, Vec<Bound>, Snapshot);

pub(super) async fn capture_with_native<T>(
    native: impl FnOnce() -> LockState + Send + 'static,
    capture: impl Future<Output = Result<T, AdapterError>>,
) -> (LockState, Result<T, AdapterError>) {
    // Drain both reads, including capture's settings restoration, on every path.
    // The fresh native query remains mandatory; it simply overlaps image work.
    let (native, capture) = tokio::join!(tokio::task::spawn_blocking(native), capture);
    (native.unwrap_or(LockState::Uncertain), capture)
}

async fn projected_targets(
    wda: &impl WdaRequest,
    key: &cache::Key,
    mut cached: Option<&Snapshot>,
    mut augment: impl FnMut(
        &str,
        &str,
        &ScreenImage,
        &ScreenImage,
        f64,
        &mut Vec<Bound>,
    ) -> Result<String, AdapterError>,
    stage: &mut AdapterDiagnostic,
) -> Result<Result<TargetCapture, AdapterError>, AdapterError> {
    loop {
        *stage = AdapterDiagnostic::TargetCapture;
        let cached_for_key = cached.filter(|snapshot| {
            snapshot.key == *key
                && snapshot.bracket_equal
                && snapshot.cost.within_visibility_budget()
                && snapshot.evidence.actionable().unwrap_or(false)
        });
        // Prove immutable replay before settings. A miss still needs fresh
        // preflight, and carries its rejected frame forward so an observed
        // A-to-B-to-A transition cannot disappear from the new bracket.
        let plan = if cached_for_key.is_some() {
            match capture_plan(wda, &key.session, cached_for_key, true).await {
                Ok(plan) => Some(plan),
                Err(failure) => {
                    if matches!(failure, CaptureFailure::HomeScreen) {
                        *stage = AdapterDiagnostic::TargetHome;
                    }
                    return Ok(Err(AdapterError));
                }
            }
        } else {
            None
        };
        let snapshot_cost = matches!(&plan, Some(CapturePlan::Snapshot(_)))
            .then(|| cached_for_key.expect("proven snapshot reuse").cost.clone());
        let work = async {
            let Captured {
                xml,
                items,
                before,
                after,
                app,
                focused,
                evidence,
                scale,
                display_id,
                reused,
            } = match plan {
                Some(CapturePlan::Snapshot(captured)) => Ok(captured),
                Some(CapturePlan::Fresh(start)) => {
                    captured_fresh(wda, &key.session, start, None, true).await
                }
                None => captured(wda, &key.session, None).await,
            }
            .map_err(|failure| {
                if matches!(failure, CaptureFailure::HomeScreen) {
                    *stage = AdapterDiagnostic::TargetHome;
                }
                AdapterError
            })?;
            let snapshot = Snapshot {
                key: key.clone(),
                cost: super::outline::xml_cost(&xml)?,
                xml: xml.clone(),
                items: items.clone(),
                app: app.clone(),
                image: after.clone(),
                evidence: evidence.clone(),
                scale,
                display_id,
                focused: focused.clone(),
                bracket_equal: before.bytes == after.bytes,
            };
            *stage = AdapterDiagnostic::TargetCandidates;
            let xml = native_visibility(&xml, &items)?;
            let mut entries = {
                let _span = crate::timing::Span::new("target_candidates");
                candidates(&xml, &items)?
            };
            for entry in &mut entries {
                entry.mapping = Some(fast::Mapping {
                    viewport: super::targets_pixels::viewport(&xml)?,
                    scale,
                    display_id,
                });
                entry.focused = focused.as_ref().map(|id| id == &entry.native);
                entry.public.can_type = matches!(
                    entry.public.kind.as_str(),
                    "XCUIElementTypeTextField"
                        | "XCUIElementTypeTextView"
                        | "XCUIElementTypeSearchField"
                ) && focused.as_deref() == Some(&entry.native);
            }
            *stage = AdapterDiagnostic::TargetMapping;
            bind_regions(&xml, &mut entries, &before, &after)?;
            *stage = AdapterDiagnostic::TargetOcr;
            let xml = augment(
                &format!("{:?}/{}/{}", key.runner, key.session, app),
                &xml,
                &before,
                &after,
                scale,
                &mut entries,
            )?;
            let raw = RawObservation {
                connected_usb: key.device.clone(),
                wda_target: key.device.clone(),
                window: evidence.window,
                state: DeviceState {
                    lock: classify_lock(&app, &xml, Some(false)),
                    wda_ready: true,
                    foreground_app: Some(app),
                },
                accessibility_tree: xml,
                screenshot: after,
            };
            if raw.state.lock != LockState::Unlocked {
                return Err(AdapterError);
            }
            Ok((raw, entries, snapshot, reused))
        };
        let result = match snapshot_cost {
            Some(cost) => budget::snapshot_context(&cost, work).await?,
            None => target_context(wda, &key.session, work).await?,
        };
        let (raw, entries, snapshot, was_reused) = match result {
            Ok(result) => result,
            Err(error) => {
                if error == TargetWorkError::Budget {
                    *stage = AdapterDiagnostic::TargetBudget;
                }
                return Ok(Err(AdapterError));
            }
        };
        // Projection and settings restoration spend the original evidence's age.
        // A reused snapshot gets at most one fresh recapture, never a new timestamp.
        if was_reused {
            match snapshot.evidence.actionable() {
                Ok(false) => {
                    cached = None;
                    continue;
                }
                Err(error) => return Ok(Err(error)),
                Ok(true) => {}
            }
        }
        return Ok(Ok((raw, entries, snapshot)));
    }
}

fn snapshot_effect(input: &Input, snapshot: &Snapshot) -> Result<bool, AdapterError> {
    if snapshot.key != input.key
        || snapshot.app != input.app
        || !crate::wda_ui_admission::safe_context(&snapshot.xml)
    {
        return Err(AdapterError);
    }
    tap_effect(
        &input.bound,
        &snapshot.xml,
        &snapshot.items,
        snapshot.focused.as_deref(),
    )
}

fn recover_ocr_capture<T>(
    mut capture: impl FnMut(bool) -> (Result<T, AdapterError>, bool),
    mut expired: impl FnMut() -> bool,
) -> Result<T, AdapterError> {
    let (result, retryable) = capture(false);
    if expired() {
        return Err(AdapterError);
    }
    if result.is_ok() || !retryable {
        return result;
    }
    let (result, _) = capture(true);
    if expired() { Err(AdapterError) } else { result }
}

impl WdaAdapter {
    pub(super) fn finish_target_capture(
        &mut self,
        token: u64,
    ) -> handkite_core::controller::TargetCompletion {
        use handkite_core::controller::{TargetCompletion, TextReadback};
        if self
            .focused_text
            .as_ref()
            .is_some_and(|input| input.token == token)
            || self
                .target_readback
                .as_ref()
                .is_some_and(|input| input.bound.ocr.is_some())
        {
            let readback = self.read_target_effect(token);
            let _next = crate::timing::Span::new("target_next_capture");
            let next = self.capture_targets();
            return TargetCompletion { readback, next };
        }
        let Some(input) = self
            .target_readback
            .take()
            .filter(|input| input.token == token)
        else {
            return TargetCompletion {
                readback: Err(AdapterError),
                next: Err(AdapterError),
            };
        };
        if input.bound.ocr.is_none() && matches!(input.action, ElementAction::Tap { .. }) {
            return self.finish_scoped_tap(input);
        }
        // Post-ack proof must be new, even if the screen did not change.
        self.target_snapshot = None;
        let next = self.capture_targets();
        let readback = next
            .as_ref()
            .map_err(|_| AdapterError)
            .and_then(|(raw, _)| {
                let snapshot = self.target_snapshot.as_ref().ok_or(AdapterError)?;
                if self.cache_key().as_ref() != Some(&input.key) {
                    return Err(AdapterError);
                }
                let input_verified = snapshot_effect(&input, snapshot)?;
                Ok(TextReadback {
                    image: RawImageObservation {
                        connected_usb: raw.connected_usb.clone(),
                        wda_target: raw.wda_target.clone(),
                        window: raw.window.clone(),
                        screenshot: raw.screenshot.clone(),
                    },
                    input_verified,
                })
            });
        handkite_core::controller::TargetCompletion { readback, next }
    }

    fn finish_scoped_tap(&mut self, input: Input) -> handkite_core::controller::TargetCompletion {
        if self.owned_runner.is_some() {
            return self.defer_scoped_tap(input);
        }
        use handkite_core::controller::{TargetCompletion, TextReadback};
        self.target_snapshot = None;
        self.targets = None;
        let result = (|| {
            self.validate_owned()?;
            if self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
            let wda = UsbWda::new(self.usb.clone())
                .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
            let device = input.key.device.clone();
            let native_started = Instant::now();
            let (lock, result) =
                self.runtime
                    .as_ref()
                    .expect("adapter runtime")
                    .block_on(capture_with_native(
                        move || native_lock::query(&device).state,
                        target_context(&wda, &input.key.session, fast::after(&wda, &input)),
                    ));
            self.cache
                .refresh_native(Some(input.key.clone()), native_started, || lock);
            let result = result.map_err(|e| {
                self.text_response_poisoned = true;
                e
            })?;
            let (raw, entries, verified, started) = self.target_work(result)?;
            if lock != LockState::Unlocked {
                return Err(AdapterError);
            }
            self.validate_owned()?;
            if self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            let public = entries.iter().map(|entry| entry.public.clone()).collect();
            self.targets = Some(Targets {
                key: input.key,
                revision: None,
                app: input.app,
                until: started + AGE,
                entries,
            });
            Ok((raw, public, verified))
        })();
        match result {
            Ok((raw, targets, verified)) => TargetCompletion {
                readback: Ok(TextReadback {
                    image: RawImageObservation {
                        connected_usb: raw.connected_usb.clone(),
                        wda_target: raw.wda_target.clone(),
                        window: raw.window.clone(),
                        screenshot: raw.screenshot.clone(),
                    },
                    input_verified: verified,
                }),
                next: Ok((raw, targets)),
            },
            Err(error) => TargetCompletion {
                readback: Err(error),
                next: Err(AdapterError),
            },
        }
    }

    pub(super) fn capture_targets(
        &mut self,
    ) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
        let deadline = Instant::now() + OCR_RECOVERY_PUBLICATION_BUDGET;
        let result = recover_ocr_capture(
            |fresh| {
                if fresh {
                    self.target_snapshot = None;
                    crate::timing::outcome("target_capture_ocr_retry", true);
                }
                let result = self.capture_targets_once();
                let retryable = self.error_diagnostic == Some(AdapterDiagnostic::TargetOcr)
                    && self.ocr_cache.last_failure() == Some(ocr::Failure::TimedOut)
                    && !self.text_response_poisoned;
                (result, retryable)
            },
            || Instant::now() >= deadline,
        );
        if result.is_err() {
            self.targets = None;
            self.target_snapshot = None;
            if self.error_diagnostic == Some(AdapterDiagnostic::TargetBudget) {
                self.clear_budget_targets();
            }
            self.error_diagnostic
                .get_or_insert(AdapterDiagnostic::TargetCapture);
        }
        result
    }

    fn capture_targets_once(
        &mut self,
    ) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
        self.error_diagnostic = None;
        self.ocr_cache.begin_attempt();
        self.finish_pending_effect(true)?;
        self.targets = None;
        self.target_input = None;
        self.target_readback = None;
        self.validate_owned()?;
        let key = self.cache_key().ok_or(AdapterError)?;
        let device = key.device.clone();
        let native_started = Instant::now();
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let previous = self
            .target_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.key == key)
            .cloned();
        let mut stage = AdapterDiagnostic::TargetCapture;
        let ocr_cache = &mut self.ocr_cache;
        let (native, result) =
            self.runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(capture_with_native(
                    move || native_lock::query(&device).state,
                    projected_targets(
                        &wda,
                        &key,
                        previous.as_ref(),
                        |identity, xml, before, after, scale, entries| {
                            vision::augment(ocr_cache, identity, xml, before, after, scale, entries)
                        },
                        &mut stage,
                    ),
                ));
        self.cache
            .refresh_native(Some(key.clone()), native_started, || native);
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.text_response_poisoned = true;
                self.error_diagnostic = Some(AdapterDiagnostic::TargetSettings);
                return Err(error);
            }
        };
        if native != LockState::Unlocked {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let (raw, entries, snapshot) = result.map_err(|error| {
            self.error_diagnostic = Some(stage);
            error
        })?;
        self.validate_owned()?;
        if self.cache_key().as_ref() != Some(&key) {
            return Err(AdapterError);
        }
        let until = snapshot.evidence.until();
        self.target_snapshot = Some(snapshot);
        let public = entries.iter().map(|entry| entry.public.clone()).collect();
        self.targets = Some(Targets {
            key,
            revision: None,
            app: raw.state.foreground_app.clone().ok_or(AdapterError)?,
            until,
            entries,
        });
        Ok((raw, public))
    }

    pub(super) fn bind_native_targets(&mut self, revision: Revision) -> Result<(), AdapterError> {
        let targets = self.targets.as_mut().ok_or(AdapterError)?;
        if targets.revision.is_some() || Instant::now() >= targets.until {
            return Err(AdapterError);
        }
        targets.revision = Some(revision);
        if let Some(snapshot) = &self.target_snapshot {
            self.target_delta = Some(delta::DeltaBaseline::from_full(
                revision,
                snapshot,
                &targets.entries,
            )?);
        } else if let Some(delta) = &mut self.target_delta {
            delta.revision = Some(revision);
        }
        Ok(())
    }

    pub(super) fn prepare_native_target(
        &mut self,
        revision: Revision,
        action: &ElementAction,
        baseline: &Observation,
    ) -> Result<PreparedTarget, AdapterError> {
        let _timing = crate::timing::Span::new("target_prepare");
        let rect = self.target_rect(action.target_id());
        self.settle_for_target(rect)?;
        self.error_diagnostic = Some(AdapterDiagnostic::TargetPrepare);
        self.target_input = None;
        self.target_readback = None;
        let targets = self.targets.as_ref().ok_or(AdapterError)?;
        if !targets.context_matches(
            self.cache_key().as_ref(),
            revision,
            baseline.state.foreground_app.as_deref(),
            Instant::now(),
        ) {
            return Err(AdapterError);
        }

        let bound = targets
            .entries
            .iter()
            .find(|e| e.public.target_id == action.target_id())
            .cloned()
            .ok_or(AdapterError)?;
        if bound.public.text_only && !matches!(action, ElementAction::Text { .. }) {
            return Err(AdapterError);
        }
        if matches!(action, ElementAction::Text { .. }) && !bound.public.can_type {
            return Err(AdapterError);
        }
        let key = targets.key.clone();
        let app = targets.app.clone();
        let until = targets.until;
        self.validate_owned()?;
        let prepared_lock = if matches!(action, ElementAction::Tap { .. }) {
            self.native_hint()
        } else {
            self.native_fresh(None)
        };
        if prepared_lock != LockState::Unlocked {
            return Err(AdapterError);
        }
        // Binding does not refresh observation age. The complete target check
        // runs once in send_native_target, immediately before dispatch.
        self.validate_owned()?;
        if self.cache_key().as_ref() != Some(&key) || Instant::now() >= until {
            return Err(AdapterError);
        }
        self.text_token = self.text_token.checked_add(1).ok_or(AdapterError)?;
        let text_basis = if matches!(action, ElementAction::Text { .. }) {
            self.target_snapshot
                .as_ref()
                .filter(|snapshot| {
                    snapshot.key == key
                        && snapshot.app == app
                        && snapshot.evidence.window == baseline.window
                })
                .and_then(|snapshot| text::basis(snapshot, &bound))
        } else {
            None
        };
        let input = Input {
            token: self.text_token,
            key,
            app,
            bound,
            action: action.clone(),
            until,
            text_basis,
        };
        let prepared = input.prepared(&self.expected, baseline);
        self.target_input = Some(input);
        self.error_diagnostic = None;
        Ok(prepared)
    }

    pub(super) fn send_native_target(
        &mut self,
        action: &ElementAction,
        token: u64,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        let _timing = crate::timing::Span::new("target_send");
        self.error_diagnostic = Some(AdapterDiagnostic::TargetRevalidation);
        // Consume before any dispatch, including an uncertain transport result.
        let input = self.target_input.take().ok_or(AdapterError)?;
        self.targets = None;
        if let Some(delta) = &mut self.target_delta {
            delta.consume(&input.bound.public);
        }
        if token != input.token
            || serde_json::to_value(action).ok() != serde_json::to_value(&input.action).ok()
            || deadline > input.until
            || Instant::now() >= deadline
            || self.cache_key().as_ref() != Some(&input.key)
        {
            return Err(AdapterError);
        }
        self.validate_owned_with_deadline(Some(deadline))?;
        let prepared_lock = if matches!(action, ElementAction::Tap { .. }) {
            self.native_hint()
        } else {
            self.native_fresh(Some(deadline))
        };
        if prepared_lock != LockState::Unlocked {
            return Err(AdapterError);
        }
        // Frame evidence comes only from the exact presented baseline, which
        // no longer contains the consumed handle.
        let basis = self
            .target_delta
            .as_ref()
            .and_then(|delta| delta.fast_basis(&input.key, &input.app));
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let context_wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let result = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(async {
                if matches!(action, ElementAction::Tap { .. }) && input.bound.ocr.is_none() {
                    let device = input.key.device.clone();
                    fast::revalidate_with_native(
                        &context_wda,
                        &input,
                        deadline,
                        basis,
                        move |until| native_lock::query_until(&device, until).state,
                    )
                    .await
                    .map(|result| result.map(Some))
                } else {
                    let _precheck = crate::timing::Span::new("target_text_precheck");
                    crate::timing::outcome(
                        "target_text_context_reused",
                        input.text_basis.is_some(),
                    );
                    rich_context(&context_wda, &input.key.session, ATTRS, async {
                        before_input_deadline(deadline, revalidate_input(&context_wda, &input))
                            .await
                    })
                    .await
                    .map(|result| result.map(|_| None))
                }
            });
        let proof = match result {
            Err(e) => {
                self.text_response_poisoned = true;
                return Err(e);
            }
            Ok(r) => r?,
        };
        let dispatch_deadline = proof
            .as_ref()
            .map(|p| p.write_deadline())
            .transpose()?
            .unwrap_or(deadline);
        self.validate_owned_with_deadline(Some(dispatch_deadline))?;
        if self.cache_key().as_ref() != Some(&input.key) {
            return Err(AdapterError);
        }
        if let Some(proof) = proof {
            if Instant::now() >= proof.deadline {
                return Err(AdapterError);
            }
            self.cache
                .refresh_native(Some(input.key.clone()), proof.started, || {
                    LockState::Unlocked
                });
        } else if self.native_fresh(Some(deadline)) != LockState::Unlocked {
            return Err(AdapterError);
        }
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?))
            .with_write_deadline(dispatch_deadline);
        // Once the dispatch future is entered, a timeout or transport error may
        // have followed delivery. Never report those results as not sent.
        self.error_diagnostic = None;
        let dispatch_timing = crate::timing::Span::new("target_input_dispatch");
        let receipt = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(dispatch_target(
                &wda,
                &mut self.actions,
                &input,
                dispatch_deadline,
            ))?;
        drop(dispatch_timing);
        if let ElementAction::Text { value, .. } = action {
            self.focused_text = Some(super::focused_text::Input {
                token,
                key: input.key.clone(),
                field: input.bound.native.clone(),
                before: input.bound.attrs.get("value").cloned().unwrap_or_default(),
                text: value.clone(),
                app: input.app.clone(),
                until: input.until,
                sent: true,
            });
        }
        self.validate_owned().inspect_err(|_| {
            self.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
        })?;
        if matches!(action, ElementAction::Tap { .. }) {
            self.target_readback = Some(input);
        }
        Ok(receipt)
    }

    pub(super) fn read_target_effect(
        &mut self,
        token: u64,
    ) -> Result<handkite_core::controller::TextReadback, AdapterError> {
        if self
            .focused_text
            .as_ref()
            .is_some_and(|input| input.token == token)
        {
            return self.read_focus(token);
        }
        let input = self
            .target_readback
            .take()
            .filter(|input| input.token == token)
            .ok_or(AdapterError)?;
        self.validate_owned()?;
        if self.cache_key().as_ref() != Some(&input.key)
            || self.native_fresh(None) != LockState::Unlocked
        {
            return Err(AdapterError);
        }
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let start = unix_ms()?;
        if input.bound.ocr.is_some() {
            let (image, verified) =
                self.runtime
                    .as_ref()
                    .expect("adapter runtime")
                    .block_on(vision::readback(
                        &wda,
                        &input.key.session,
                        &input.app,
                        &input.bound,
                    ))?;
            self.validate_owned()?;
            if self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            return Ok(handkite_core::controller::TextReadback {
                image: RawImageObservation {
                    connected_usb: self.expected.clone(),
                    wda_target: self.expected.clone(),
                    window: ObservationWindow {
                        started_unix_ms: start,
                        finished_unix_ms: unix_ms()?,
                    },
                    screenshot: image,
                },
                input_verified: verified,
            });
        }
        let result = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(target_context(&wda, &input.key.session, async {
                let Captured {
                    xml,
                    items,
                    after: image,
                    app,
                    focused,
                    evidence,
                    ..
                } = captured(&wda, &input.key.session, None).await?;
                if app != input.app || !crate::wda_ui_admission::safe_context(&xml) {
                    return Err(AdapterError);
                }
                let verified = tap_effect(&input.bound, &xml, &items, focused.as_deref())?;
                Ok((image, verified, evidence.window))
            }));
        let (image, verified, window) = match result {
            Err(e) => {
                self.text_response_poisoned = true;
                return Err(e);
            }
            Ok(r) => self.target_work(r)?,
        };
        self.validate_owned()?;
        if self.cache_key().as_ref() != Some(&input.key) {
            return Err(AdapterError);
        }
        Ok(handkite_core::controller::TextReadback {
            image: RawImageObservation {
                connected_usb: self.expected.clone(),
                wda_target: self.expected.clone(),
                window,
                screenshot: image,
            },
            input_verified: verified,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ocr_timeout_recovery_publishes_only_the_fresh_capture() {
        let mut calls = Vec::new();
        let result = super::recover_ocr_capture(
            |fresh| {
                calls.push(fresh);
                if fresh {
                    (Ok(("new targets", 2)), false)
                } else {
                    (Err(super::AdapterError), true)
                }
            },
            || false,
        )
        .unwrap();
        assert_eq!(calls, [false, true]);
        assert_eq!(result, ("new targets", 2));
    }

    #[test]
    fn ocr_recovery_never_retries_a_second_failure_or_a_nontransient_failure() {
        for retryable in [false, true] {
            let mut calls = 0;
            let result = super::recover_ocr_capture::<()>(
                |_| {
                    calls += 1;
                    (Err(super::AdapterError), retryable)
                },
                || false,
            );
            assert!(result.is_err());
            assert_eq!(calls, if retryable { 2 } else { 1 });
        }
    }

    #[test]
    fn ocr_recovery_budget_excludes_late_targets_without_cancelling_the_attempt() {
        use std::cell::Cell;
        for expire_on_check in [0, 1] {
            let mut calls = 0;
            let checks = Cell::new(0);
            let mut restored = false;
            let result = super::recover_ocr_capture(
                |fresh| {
                    calls += 1;
                    restored = true;
                    if fresh {
                        (Ok("late targets"), false)
                    } else {
                        (Err(super::AdapterError), true)
                    }
                },
                || {
                    let check = checks.get();
                    checks.set(check + 1);
                    check >= expire_on_check
                },
            );
            assert!(result.is_err());
            assert!(restored);
            assert_eq!(calls, expire_on_check + 1);
        }
    }
    use super::*;
    use std::cell::{Cell, RefCell};

    #[tokio::test]
    async fn fresh_native_capture_overlaps_and_drains_both_sides() {
        let (started, receive) = tokio::sync::oneshot::channel();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture_completed = completed.clone();
        let (lock, result) = capture_with_native(
            move || {
                started.send(()).unwrap();
                while !completed.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                LockState::Unlocked
            },
            async move {
                receive.await.unwrap();
                capture_completed.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            },
        )
        .await;
        assert_eq!(lock, LockState::Unlocked);
        assert!(result.is_ok());

        for state in [LockState::Locked, LockState::Uncertain] {
            let restored = Cell::new(false);
            let (lock, result) = capture_with_native(move || state, async {
                tokio::task::yield_now().await;
                restored.set(true);
                Err::<(), _>(AdapterError)
            })
            .await;
            assert_eq!(lock, state);
            assert!(result.is_err());
            assert!(restored.get());
        }
    }

    #[tokio::test]
    async fn capture_failure_still_joins_fresh_native_read() {
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = completed.clone();
        let (lock, result) = capture_with_native(
            move || {
                std::thread::sleep(Duration::from_millis(20));
                marker.store(true, std::sync::atomic::Ordering::Release);
                LockState::Unlocked
            },
            async { Err::<(), _>(AdapterError) },
        )
        .await;
        assert_eq!(lock, LockState::Unlocked);
        assert!(result.is_err());
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    }
    const XML: &str = r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="400" height="800"><XCUIElementTypeButton name="choose" label="Choose" visible="true" enabled="true" x="10" y="20" width="100" height="40"/></XCUIElementTypeApplication></AppiumAUT>"#;
    fn item() -> Value {
        json!({"ELEMENT":"native-one","element-6066-11e4-a52e-4f735466cecf":"native-one","type":"XCUIElementTypeButton","label":"Choose","attribute/name":"choose","attribute/value":null,"attribute/placeholderValue":null,"attribute/focused":false,"attribute/hittable":true,"attribute/selected":false,"enabled":true,"displayed":true,"rect":{"x":10,"y":20,"width":100,"height":40}})
    }

    #[test]
    fn absent_target_is_a_preinput_refusal_and_replaces_old_diagnostic() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let fake = FullCapture::new();
        let action = ElementAction::Tap {
            target_id: "missing".into(),
        };
        adapter.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
        assert!(
            adapter
                .prepare_native_target(Revision(7), &action, &fake.baseline())
                .is_err()
        );
        assert_eq!(
            adapter.take_error_diagnostic(),
            Some(AdapterDiagnostic::TargetPrepare)
        );
        assert!(adapter.target_input.is_none());
    }

    #[test]
    fn invalid_target_token_is_consumed_before_any_native_access() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let runner = runtime.spawn(async {});
        let fake = FullCapture::new();
        let input = fake.input(
            cache::Key {
                device: HardwareId::new("fixture").unwrap(),
                session: "session".into(),
                runner: runner.id(),
            },
            Instant::now() + Duration::from_secs(5),
        );
        let action = input.action.clone();
        let until = input.until;
        let mut adapter = WdaAdapter::new(input.key.device.clone()).unwrap();
        adapter.target_input = Some(input);
        adapter.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
        assert!(adapter.send_native_target(&action, 18, until).is_err());
        assert_eq!(
            adapter.take_error_diagnostic(),
            Some(AdapterDiagnostic::TargetRevalidation)
        );
        assert!(adapter.target_input.is_none());
        assert!(adapter.send_native_target(&action, 17, until).is_err());
    }

    #[test]
    fn input_queries_preserve_text_coverage_and_quote_labels_as_literals() {
        let mut bound = candidates(XML, &json!([item()])).unwrap().remove(0);
        let tap = ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        };
        let label = "choice\" OR TRUEPREDICATE OR label == \"x\\y";
        bound.rich["label"] = json!(label);
        let query = input_predicate(&bound, &tap).unwrap();
        assert_eq!(
            query,
            format!(
                "type == \"XCUIElementTypeButton\" AND label == {}",
                serde_json::to_string(label).unwrap()
            )
        );
        bound.rich["label"] = Value::Null;
        assert_eq!(
            input_predicate(&bound, &tap).unwrap(),
            "type == \"XCUIElementTypeButton\""
        );
        bound.rich.as_object_mut().unwrap().remove("label");
        assert!(input_predicate(&bound, &tap).is_err());
        bound.rich["label"] = json!(17);
        assert!(input_predicate(&bound, &tap).is_err());
        let text = ElementAction::Text {
            target_id: tap.target_id().into(),
            value: "hello".into(),
        };
        let query = input_predicate(&bound, &text).unwrap();
        for kind in ["TextField", "TextView", "SearchField", "Keyboard"] {
            assert!(query.contains(&format!("XCUIElementType{kind}")));
        }
        assert!(!query.contains("label"));
    }

    struct FullCapture {
        app: &'static str,
        xml: String,
        items: Value,
        image: ScreenImage,
        settings: RefCell<Value>,
        source_reads: Cell<usize>,
        preflight_reads: Cell<usize>,
        inventory_reads: Cell<usize>,
        image_reads: Cell<usize>,
        inputs: Cell<usize>,
        overlap: bool,
        source_started: tokio::sync::Notify,
        inventory_started: tokio::sync::Notify,
        source_delay: Duration,
        setup_delay: Duration,
        restoration_delay: Duration,
        second_image_delay: Duration,
        restoration_fails: bool,
        settings_requests: Cell<usize>,
        settings_writes: Cell<usize>,
        first_app_read: Cell<Option<Instant>>,
        last_lock_read: Cell<Option<Instant>>,
        active_id: String,
        input_response_delay: Duration,
        input_response_fails: bool,
        display_change_after_images: Option<usize>,
        changed_second_image: bool,
        active_reads: Cell<usize>,
        hittable: bool,
        last_predicate: RefCell<String>,
        scale: f64,
        window_size: [f64; 2],
    }

    impl FullCapture {
        fn new() -> Self {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 400, 800);
                encoder.set_color(png::ColorType::Grayscale);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&vec![255; 400 * 800])
                    .unwrap();
            }
            Self {
                app: "example.app",
                xml: XML.into(),
                items: json!([item()]),
                image: ScreenImage {
                    format: ImageFormat::Png,
                    bytes,
                },
                settings: RefCell::new(Self::original_settings()),
                source_reads: Cell::new(0),
                preflight_reads: Cell::new(0),
                inventory_reads: Cell::new(0),
                image_reads: Cell::new(0),
                inputs: Cell::new(0),
                overlap: false,
                source_started: tokio::sync::Notify::new(),
                inventory_started: tokio::sync::Notify::new(),
                source_delay: Duration::ZERO,
                setup_delay: Duration::ZERO,
                restoration_delay: Duration::ZERO,
                second_image_delay: Duration::ZERO,
                restoration_fails: false,
                settings_requests: Cell::new(0),
                settings_writes: Cell::new(0),
                first_app_read: Cell::new(None),
                last_lock_read: Cell::new(None),
                active_id: "native-one".into(),
                input_response_delay: Duration::ZERO,
                input_response_fails: false,
                display_change_after_images: None,
                changed_second_image: false,
                active_reads: Cell::new(0),
                hittable: true,
                last_predicate: RefCell::new(String::new()),
                scale: 1.,
                window_size: [400., 800.],
            }
        }

        fn original_settings() -> Value {
            json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"name,label"})
        }

        fn input(&self, key: cache::Key, until: Instant) -> Input {
            let mut entries = candidates(XML, &json!([item()])).unwrap();
            bind_regions(XML, &mut entries, &self.image, &self.image).unwrap();
            let bound = entries.remove(0);
            Input {
                token: 17,
                text_basis: None,
                key,
                app: "example.app".into(),
                action: ElementAction::Tap {
                    target_id: bound.public.target_id.clone(),
                },
                bound,
                until,
            }
        }

        fn baseline(&self) -> Observation {
            Observation {
                device: handkite_core::protocol::DeviceKey::new("fixture").unwrap(),
                revision: Revision(7),
                window: ObservationWindow {
                    started_unix_ms: 100,
                    finished_unix_ms: 200,
                },
                state: DeviceState {
                    lock: LockState::Unlocked,
                    wda_ready: true,
                    foreground_app: Some("example.app".into()),
                },
                accessibility_tree: XML.into(),
                screenshot: self.image.clone(),
            }
        }

        fn snapshot(&self, key: &cache::Key, age: Duration) -> Snapshot {
            let now = Instant::now();
            let now_ms = unix_ms().unwrap();
            Snapshot {
                key: key.clone(),
                cost: super::super::outline::xml_cost(&self.xml).unwrap(),
                xml: self.xml.clone(),
                items: self.items.clone(),
                app: "example.app".into(),
                image: self.image.clone(),
                scale: 1.0,
                display_id: None,
                focused: None,
                bracket_equal: true,
                evidence: EvidenceWindow {
                    window: ObservationWindow {
                        started_unix_ms: now_ms - age.as_millis() as u64 - 20,
                        finished_unix_ms: now_ms - age.as_millis() as u64,
                    },
                    started: now - age - Duration::from_millis(20),
                    finished: now - age,
                },
            }
        }
    }

    impl WdaRequest for FullCapture {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let result = if path == "/status" || path == "/session/session" {
                json!({})
            } else if path.ends_with("/appium/settings") {
                let requests = self.settings_requests.get();
                self.settings_requests.set(requests + 1);
                if requests == 0 && !self.setup_delay.is_zero() {
                    tokio::time::sleep(self.setup_delay).await;
                }
                if method == "POST" {
                    self.settings_writes.set(self.settings_writes.get() + 1);
                    let settings =
                        serde_json::from_slice::<Value>(body).unwrap()["settings"].clone();
                    let restoring = settings == Self::original_settings();
                    if restoring && !self.restoration_delay.is_zero() {
                        tokio::time::sleep(self.restoration_delay).await;
                    }
                    if !restoring || !self.restoration_fails {
                        *self.settings.borrow_mut() = settings;
                    }
                }
                let mut result = self.settings.borrow().clone();
                if let Some(after) = self.display_change_after_images {
                    result["currentDisplayId"] = json!(if self.image_reads.get() >= after {
                        2
                    } else {
                        1
                    });
                }
                result
            } else if path == SOURCE {
                // Cost preflight runs before the rich transaction; only the
                // paired source participates in the inventory overlap fixture.
                if self.settings.borrow()["shouldUseCompactResponses"] == true {
                    self.preflight_reads.set(self.preflight_reads.get() + 1);
                    return Ok(HttpResponse {
                        status: 200,
                        body: serde_json::to_vec(&json!({"value":self.xml})).unwrap(),
                    });
                }
                self.source_reads.set(self.source_reads.get() + 1);
                if self.overlap {
                    self.source_started.notify_one();
                    self.inventory_started.notified().await;
                }
                if !self.source_delay.is_zero() {
                    tokio::time::sleep(self.source_delay).await;
                }
                json!(self.xml)
            } else if path.ends_with("/elements") {
                assert_eq!(method, "POST");
                *self.last_predicate.borrow_mut() = serde_json::from_slice::<Value>(body).unwrap()
                    ["value"]
                    .as_str()
                    .unwrap()
                    .into();
                self.inventory_reads.set(self.inventory_reads.get() + 1);
                if self.overlap {
                    self.inventory_started.notify_one();
                    self.source_started.notified().await;
                }
                self.items.clone()
            } else if path == "/screenshot" {
                self.image_reads.set(self.image_reads.get() + 1);
                if self.image_reads.get() == 2 && !self.second_image_delay.is_zero() {
                    tokio::time::sleep(self.second_image_delay).await;
                }
                if self.changed_second_image && self.image_reads.get() == 2 {
                    let mut bytes = Vec::new();
                    let mut pixels = vec![255; 400 * 800];
                    pixels[25 * 400 + 25] = 0;
                    let mut encoder = png::Encoder::new(&mut bytes, 400, 800);
                    encoder.set_color(png::ColorType::Grayscale);
                    encoder.set_depth(png::BitDepth::Eight);
                    encoder
                        .write_header()
                        .unwrap()
                        .write_image_data(&pixels)
                        .unwrap();
                    json!(base64::engine::general_purpose::STANDARD.encode(bytes))
                } else {
                    json!(base64::engine::general_purpose::STANDARD.encode(&self.image.bytes))
                }
            } else if path.ends_with("/wda/activeAppInfo") {
                if self.first_app_read.get().is_none() {
                    self.first_app_read.set(Some(Instant::now()));
                }
                json!({"bundleId":self.app})
            } else if path.ends_with("/wda/locked") {
                self.last_lock_read.set(Some(Instant::now()));
                json!(false)
            } else if path.ends_with("/element/active") {
                self.active_reads.set(self.active_reads.get() + 1);
                json!({"ELEMENT":self.active_id})
            } else if path == "/wda/screen" {
                json!({"scale":self.scale})
            } else if path == "/wda/screens" {
                json!([{"displayId":1,"scale":1.0}])
            } else if path.ends_with("/window/size") {
                json!({"width":self.window_size[0],"height":self.window_size[1]})
            } else if path.ends_with("/attribute/hittable") {
                json!(self.hittable)
            } else if path.ends_with("/wda/keys") {
                assert_eq!(method, "POST");
                assert_eq!(*self.settings.borrow(), Self::original_settings());
                self.inputs.set(self.inputs.get() + 1);
                Value::Null
            } else if path.ends_with("/element/native-one/click") {
                assert_eq!(method, "POST");
                assert_eq!(*self.settings.borrow(), Self::original_settings());
                self.inputs.set(self.inputs.get() + 1);
                tokio::time::sleep(self.input_response_delay).await;
                if self.input_response_fails {
                    return Err(crate::wda_transport::WdaError::TimedOutAfterSent);
                }
                Value::Null
            } else {
                panic!("unexpected request: {method} {path}");
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":result})).unwrap(),
            })
        }
    }

    async fn checked_dispatch(
        fake: &FullCapture,
        input: &Input,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        rich_context(
            fake,
            &input.key.session,
            ATTRS,
            before_input_deadline(deadline, revalidate_input(fake, input)),
        )
        .await??;
        let mut actions = WdaActions::bound_to_existing(&input.key.session)?;
        dispatch_target(fake, &mut actions, input, deadline).await
    }

    #[tokio::test]
    async fn oversized_target_capture_never_enters_settings_even_with_cached_snapshot() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let mut fake = FullCapture::new();
        for nodes in [351, 1881] {
            fake.xml = format!(
                "<AppiumAUT>{}</AppiumAUT>",
                "<XCUIElementTypeOther/>".repeat(nodes)
            );
            let cached = fake.snapshot(&key, Duration::ZERO);
            for snapshot in [None, Some(&cached)] {
                let mut stage = AdapterDiagnostic::TargetCapture;
                let result = projected_targets(
                    &fake,
                    &key,
                    snapshot,
                    |_, _, _, _, _, _| panic!("budget must refuse before OCR"),
                    &mut stage,
                )
                .await;
                assert!(result.unwrap().is_err());
                assert_eq!(stage, AdapterDiagnostic::TargetBudget);
                assert_eq!(fake.settings_requests.get(), 0);
                assert_eq!(fake.inventory_reads.get(), 0);
                assert_eq!(fake.image_reads.get(), 0);
                assert_eq!(fake.inputs.get(), 0);
            }
        }
        assert_eq!(fake.preflight_reads.get(), 4);
        fake.xml = XML.into();
        assert!(
            projected_targets(
                &fake,
                &key,
                None,
                fixture_projection,
                &mut AdapterDiagnostic::TargetCapture
            )
            .await
            .unwrap()
            .is_ok()
        );
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        assert_eq!(fake.inventory_reads.get(), 1);
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[test]
    fn budget_refusal_invalidates_all_prepared_and_cached_target_authority() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let task = adapter.runtime.as_ref().unwrap().spawn(async {});
        let key = fixture_key(task.id());
        let fake = FullCapture::new();
        let snapshot = fake.snapshot(&key, Duration::ZERO);
        let input = fake.input(key.clone(), Instant::now() + AGE);
        let entries = vec![input.bound.clone()];
        adapter.target_delta =
            Some(delta::DeltaBaseline::from_full(Revision(7), &snapshot, &entries).unwrap());
        adapter.target_snapshot = Some(snapshot);
        adapter.targets = Some(Targets {
            key,
            revision: Some(Revision(7)),
            app: input.app.clone(),
            until: input.until,
            entries,
        });
        adapter.target_input = Some(input.clone());
        adapter.target_readback = Some(input);
        assert_eq!(
            adapter.target_work::<()>(Err(TargetWorkError::Budget)),
            Err(AdapterError)
        );
        assert_eq!(
            adapter.error_diagnostic,
            Some(AdapterDiagnostic::TargetBudget)
        );
        assert!(!adapter.text_response_poisoned);
        assert!(adapter.targets.is_none());
        assert!(adapter.target_snapshot.is_none());
        assert!(adapter.target_delta.is_none());
        assert!(adapter.target_input.is_none());
        assert!(adapter.target_readback.is_none());
        assert!(adapter.target_rect("unused").is_none());
    }

    #[tokio::test]
    async fn target_ack_after_send_deadline_is_preserved_without_replay() {
        let runner = tokio::spawn(async {});
        for response_fails in [false, true] {
            let mut fake = FullCapture::new();
            fake.input_response_delay = Duration::from_millis(120);
            fake.input_response_fails = response_fails;
            let deadline = Instant::now() + Duration::from_millis(100);
            let input = fake.input(fixture_key(runner.id()), deadline);
            let mut actions = WdaActions::bound_to_existing(&input.key.session).unwrap();
            let result = dispatch_target(&fake, &mut actions, &input, deadline).await;
            assert!(Instant::now() > deadline);
            assert_eq!(fake.inputs.get(), 1);
            assert_eq!(result.is_ok(), !response_fails);
            if let Ok(receipt) = result {
                assert_eq!(receipt.accepted, Some(true));
            }
        }
        let fake = FullCapture::new();
        let input = fake.input(fixture_key(runner.id()), Instant::now());
        let mut actions = WdaActions::bound_to_existing(&input.key.session).unwrap();
        assert!(
            dispatch_target(&fake, &mut actions, &input, input.until)
                .await
                .is_err()
        );
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn display_change_after_final_image_rejects_fresh_and_cached_capture() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        for reuse in [false, true] {
            let mut fake = FullCapture::new();
            fake.display_change_after_images = Some(2);
            let mut snapshot = fake.snapshot(&key, Duration::ZERO);
            snapshot.display_id = Some(1);
            let result = projected_targets(
                &fake,
                &key,
                reuse.then_some(&snapshot),
                fixture_projection,
                &mut AdapterDiagnostic::TargetCapture,
            )
            .await
            .unwrap();
            assert!(result.is_err());
            assert_eq!(fake.image_reads.get(), 2);
            assert_eq!(fake.inputs.get(), 0);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        }
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn one_full_revalidation_overlaps_reads_before_one_native_input() {
        let runner = tokio::spawn(async {});
        let mut fake = FullCapture::new();
        fake.overlap = true;
        let input = fake.input(
            cache::Key {
                device: HardwareId::new("fixture").unwrap(),
                session: "session".into(),
                runner: runner.id(),
            },
            Instant::now() + Duration::from_secs(5),
        );
        let baseline = fake.baseline();
        let prepared = input.prepared(&input.key.device, &baseline);
        assert_eq!(prepared.evidence.window, baseline.window);
        assert_eq!(prepared.evidence.valid_until, input.until);
        let receipt = tokio::time::timeout(
            Duration::from_secs(2),
            checked_dispatch(&fake, &input, prepared.evidence.valid_until),
        )
        .await
        .expect("source and inventory must make progress together")
        .unwrap();
        assert_eq!(receipt.accepted, Some(true));
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inventory_reads.get(), 1);
        assert_eq!(fake.image_reads.get(), 2);
        assert_eq!(fake.inputs.get(), 1);
        assert_eq!(fake.active_reads.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn home_capture_is_classified_before_images_without_input() {
        let task = tokio::spawn(async {});
        let mut fake = FullCapture::new();
        fake.app = "com.apple.springboard";
        let mut stage = AdapterDiagnostic::TargetCapture;
        let result = projected_targets(
            &fake,
            &fixture_key(task.id()),
            None,
            |_, _, _, _, _, _| panic!("Home must not mint targets"),
            &mut stage,
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(stage.code(), "AdapterTargetHome");
        assert_eq!(fake.image_reads.get(), 0);
        assert_eq!(fake.source_reads.get(), 0);
        assert_eq!(fake.inputs.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn home_capture_does_not_hide_failed_settings_restoration() {
        let task = tokio::spawn(async {});
        let mut fake = FullCapture::new();
        fake.app = "com.apple.springboard";
        fake.restoration_fails = true;
        let mut stage = AdapterDiagnostic::TargetCapture;
        assert!(
            projected_targets(
                &fake,
                &fixture_key(task.id()),
                None,
                |_, _, _, _, _, _| panic!("Home must not mint targets"),
                &mut stage,
            )
            .await
            .is_err()
        );
        assert_eq!(fake.image_reads.get(), 0);
        assert_eq!(fake.inputs.get(), 0);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn observation_still_reads_active_identity() {
        let fake = FullCapture::new();
        captured(&fake, "session", None).await.unwrap();
        assert_eq!(fake.active_reads.get(), 1);
        assert_eq!(fake.inputs.get(), 0);
    }

    #[tokio::test]
    async fn focus_gain_proves_a_field_tap_even_when_the_keyboard_moves_it() {
        let runner = tokio::spawn(async {});
        let fake = FullCapture::new();
        let mut bound = fake
            .input(fixture_key(runner.id()), Instant::now() + AGE)
            .bound;
        bound.public.kind = "XCUIElementTypeTextField".into();
        bound.rich["type"] = json!(bound.public.kind);
        bound.attrs.insert("type".into(), bound.public.kind.clone());
        bound.focused = Some(false);
        let mut current = bound.rich.clone();
        current["rect"]["width"] = json!(80.);
        let xml = XML.replace("Button", "TextField");
        assert!(tap_effect(&bound, &xml, &json!([current.clone()]), Some(&bound.native)).unwrap());
        assert!(!tap_effect(&bound, &xml, &json!([current.clone()]), None).unwrap());
        current["label"] = json!("Another field");
        assert!(!tap_effect(&bound, &xml, &json!([current]), Some(&bound.native)).unwrap());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn original_expiry_is_not_refreshed_and_prevents_input() {
        let runner = tokio::spawn(async {});
        let fake = FullCapture::new();
        let original_expiry = Instant::now() - Duration::from_millis(1);
        let input = fake.input(
            cache::Key {
                device: HardwareId::new("fixture").unwrap(),
                session: "session".into(),
                runner: runner.id(),
            },
            original_expiry,
        );
        let baseline = fake.baseline();
        let prepared = input.prepared(&input.key.device, &baseline);
        assert_eq!(prepared.evidence.window, baseline.window);
        assert_eq!(prepared.evidence.valid_until, original_expiry);
        assert!(
            checked_dispatch(&fake, &input, prepared.evidence.valid_until)
                .await
                .is_err()
        );
        assert_eq!(fake.source_reads.get(), 0);
        assert_eq!(fake.inventory_reads.get(), 0);
        assert_eq!(fake.inputs.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn expiry_during_revalidation_and_stale_target_never_send_input() {
        let runner = tokio::spawn(async {});
        for expired in [false, true] {
            let mut fake = FullCapture::new();
            let input = fake.input(
                cache::Key {
                    device: HardwareId::new("fixture").unwrap(),
                    session: "session".into(),
                    runner: runner.id(),
                },
                Instant::now() + Duration::from_secs(5),
            );
            let deadline = if expired {
                fake.source_delay = Duration::from_millis(100);
                Instant::now() + Duration::from_millis(20)
            } else {
                fake.items[0]["ELEMENT"] = json!("remapped");
                fake.items[0]["element-6066-11e4-a52e-4f735466cecf"] = json!("remapped");
                input.until
            };
            assert!(checked_dispatch(&fake, &input, deadline).await.is_err());
            assert_eq!(fake.source_reads.get(), 1);
            assert_eq!(fake.inputs.get(), 0);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        }
        runner.await.unwrap();
    }
    #[test]
    fn visibility_is_projected_only_from_a_unique_native_identity() {
        let xml = XML.replace(" visible=\"true\"", "");
        let projected = native_visibility(&xml, &json!([item()])).unwrap();
        assert_eq!(candidates(&projected, &json!([item()])).unwrap().len(), 1);
        let mut hidden = item();
        hidden["displayed"] = json!(false);
        assert!(
            native_visibility(&xml, &json!([hidden]))
                .unwrap()
                .contains("visible=\"false\"")
        );
        assert!(
            native_visibility(&xml, &json!([item(), item()]))
                .unwrap()
                .contains("visible=\"false\"")
        );
        let mut disabled = item();
        disabled["enabled"] = json!(false);
        assert!(
            native_visibility(&xml, &json!([disabled]))
                .unwrap()
                .contains("visible=\"false\"")
        );
        assert!(
            !native_visibility(&xml, &json!([]))
                .unwrap()
                .contains("visible=")
        );
    }

    #[test]
    fn text_context_requires_all_fields_including_hidden_disabled_and_keyboard() {
        let mut field = item();
        field["type"] = json!("XCUIElementTypeTextView");
        let xml = XML.replace("Button", "TextView");
        assert!(complete_text_context(&xml, &json!([field.clone()])));
        assert!(!complete_text_context(&xml, &json!([])));
        assert!(!complete_text_context(
            &xml,
            &json!([field.clone(), field.clone()])
        ));
        let mut hidden = field.clone();
        hidden["enabled"] = json!(false);
        hidden["displayed"] = json!(false);
        hidden["rect"]["x"] = json!(120);
        let extra = xml.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeTextView name=\"choose\" label=\"Choose\" visible=\"false\" enabled=\"false\" x=\"120\" y=\"20\" width=\"100\" height=\"40\"/></XCUIElementTypeApplication>");
        assert!(!complete_text_context(&extra, &json!([field.clone()])));
        assert!(complete_text_context(
            &extra,
            &json!([field.clone(), hidden.clone()])
        ));
        hidden["enabled"] = json!(true);
        assert!(!complete_text_context(
            &extra,
            &json!([field.clone(), hidden])
        ));
        let mut keyboard = field;
        keyboard["type"] = json!("XCUIElementTypeKeyboard");
        keyboard["label"] = Value::Null;
        keyboard["attribute/name"] = Value::Null;
        let keyboard_xml = xml
            .replace("TextView", "Keyboard")
            .replace(" name=\"choose\" label=\"Choose\"", "");
        assert!(complete_text_context(
            &keyboard_xml,
            &json!([keyboard.clone()])
        ));
        keyboard["displayed"] = json!(false);
        assert!(!complete_text_context(
            &keyboard_xml,
            &json!([keyboard.clone()])
        ));
        keyboard["displayed"] = json!(true);
        assert!(!complete_text_context(
            &keyboard_xml.replace("visible=\"true\"", "visible=\"false\""),
            &json!([keyboard.clone()])
        ));
        keyboard.as_object_mut().unwrap().remove("label");
        assert!(!complete_text_context(&keyboard_xml, &json!([keyboard])));
    }

    fn fixture_projection(
        _: &str,
        xml: &str,
        _: &ScreenImage,
        _: &ScreenImage,
        _: f64,
        _: &mut Vec<Bound>,
    ) -> Result<String, AdapterError> {
        Ok(xml.into())
    }

    fn fixture_key(runner: tokio::task::Id) -> cache::Key {
        cache::Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "session".into(),
            runner,
        }
    }

    fn accept_fixture_capture(
        raw: RawObservation,
    ) -> Result<Observation, handkite_core::controller::ControllerError> {
        struct Adapter(Option<RawObservation>);
        impl DeviceAdapter for Adapter {
            fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
                let raw = self.0.as_ref().ok_or(AdapterError)?;
                Ok((raw.connected_usb.clone(), raw.wda_target.clone()))
            }
            fn observe(&mut self) -> Result<RawObservation, AdapterError> {
                self.0.take().ok_or(AdapterError)
            }
            fn observe_targets(
                &mut self,
            ) -> Result<(RawObservation, Vec<ElementTarget>), AdapterError> {
                Ok((self.observe()?, vec![]))
            }
            fn bind_targets(&mut self, _: Revision) -> Result<(), AdapterError> {
                Ok(())
            }
            fn execute(
                &mut self,
                _: &Action,
                _: Instant,
            ) -> Result<TransportReceipt, AdapterError> {
                panic!("observation must not send input")
            }
        }
        let key = handkite_core::protocol::DeviceKey::new("fixture").unwrap();
        let binding = handkite_core::protocol::DeviceBinding {
            key: key.clone(),
            usb_id: raw.connected_usb.clone(),
        };
        handkite_core::controller::Controller::new(
            binding,
            Adapter(Some(raw)),
            CACHE_WINDOW_MS,
            CACHE_WINDOW_MS,
        )
        .observe_targets(&key)
        .map(|result| result.observation)
    }

    #[tokio::test]
    async fn evidence_window_excludes_setup_projection_and_restoration() {
        let runner = tokio::spawn(async {});
        let mut fake = FullCapture::new();
        fake.setup_delay = Duration::from_millis(CACHE_WINDOW_MS + 50);
        fake.source_delay = Duration::from_millis(10);
        fake.restoration_delay = Duration::from_millis(20);
        let started = Instant::now();
        let (raw, _, snapshot) = projected_targets(
            &fake,
            &fixture_key(runner.id()),
            None,
            |identity, xml, before, after, scale, entries| {
                std::thread::sleep(Duration::from_millis(20));
                fixture_projection(identity, xml, before, after, scale, entries)
            },
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() > Duration::from_millis(CACHE_WINDOW_MS));
        assert!(snapshot.evidence.started.duration_since(started) >= fake.setup_delay);
        assert!(snapshot.evidence.started <= fake.first_app_read.get().unwrap());
        assert!(snapshot.evidence.finished >= fake.last_lock_read.get().unwrap());
        assert!(snapshot.evidence.finished.elapsed() >= fake.restoration_delay);
        assert_eq!(raw.window, snapshot.evidence.window);
        assert!(raw.window.duration_ms().unwrap() < CACHE_WINDOW_MS);
        assert!(accept_fixture_capture(raw).is_ok());
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn processing_expiry_is_not_hidden_by_the_window_end() {
        let runner = tokio::spawn(async {});
        let fake = FullCapture::new();
        let (raw, _, snapshot) = projected_targets(
            &fake,
            &fixture_key(runner.id()),
            None,
            |identity, xml, before, after, scale, entries| {
                std::thread::sleep(Duration::from_millis(CACHE_WINDOW_MS + 50));
                fixture_projection(identity, xml, before, after, scale, entries)
            },
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(raw.window, snapshot.evidence.window);
        assert!(!snapshot.evidence.reusable().unwrap());
        assert!(matches!(
            accept_fixture_capture(raw),
            Err(handkite_core::controller::ControllerError::StaleObservation)
        ));
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inputs.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn target_publication_recaptures_a_snapshot_near_original_expiry() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let fake = FullCapture::new();
        let original = fake.snapshot(&key, Duration::from_millis(4500));
        assert!(original.evidence.reusable().unwrap());
        assert!(!original.evidence.actionable().unwrap());
        let (raw, _, fresh) = projected_targets(
            &fake,
            &key,
            Some(&original),
            fixture_projection,
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(raw.window, original.evidence.window);
        assert!(fresh.evidence.actionable().unwrap());
        assert_eq!(fake.preflight_reads.get(), 1);
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inventory_reads.get(), 1);
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[test]
    fn publication_headroom_bounds_both_original_clocks_without_renewing_them() {
        let now = Instant::now();
        let window = EvidenceWindow {
            window: ObservationWindow {
                started_unix_ms: 9900,
                finished_unix_ms: 10000,
            },
            started: now - Duration::from_millis(100),
            finished: now,
        };
        let limit = CACHE_WINDOW_MS - ACTION_HEADROOM_MS;
        assert!(window.actionable_at(now + Duration::from_millis(limit - 1), 10000 + limit - 1));
        assert!(!window.actionable_at(now + Duration::from_millis(limit), 10000));
        assert!(!window.actionable_at(now, 10000 + limit));
        assert!(window.reusable_at(now + Duration::from_millis(limit), 10000 + limit));
        assert_eq!(window.window.finished_unix_ms, 10000);
        assert_eq!(window.finished, now);
    }

    #[tokio::test]
    async fn immutable_snapshot_replay_skips_only_the_budget_and_enumeration_reads() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let mut fake = FullCapture::new();
        let cached = fake.snapshot(&key, Duration::from_millis(100));
        fake.restoration_fails = true;
        // Unseen hierarchy growth cannot trigger a fresh broad enumeration
        // through a count hint: this path only replays bounded original data.
        fake.xml = format!(
            "<AppiumAUT>{}</AppiumAUT>",
            "<XCUIElementTypeOther/>".repeat(1881)
        );
        let (raw, _, replayed) = projected_targets(
            &fake,
            &key,
            Some(&cached),
            fixture_projection,
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(raw.window, cached.evidence.window);
        assert_eq!(replayed.xml, cached.xml);
        assert_eq!(replayed.evidence.until(), cached.evidence.until());
        assert_eq!(replayed.cost.nodes, cached.cost.nodes);
        assert_eq!(fake.settings_writes.get(), 0);
        assert_eq!(fake.preflight_reads.get(), 0);
        assert_eq!(fake.source_reads.get(), 0);
        assert_eq!(fake.inventory_reads.get(), 0);
        assert_eq!(fake.image_reads.get(), 2);
        assert_eq!(fake.inputs.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn changed_same_app_frame_cannot_lend_a_small_snapshot_count_to_city() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let mut fake = FullCapture::new();
        let cached = fake.snapshot(&key, Duration::ZERO);
        fake.image.bytes.push(0);
        fake.xml = format!(
            "<AppiumAUT>{}</AppiumAUT>",
            "<XCUIElementTypeOther/>".repeat(1881)
        );
        let mut stage = AdapterDiagnostic::TargetCapture;
        assert!(
            projected_targets(
                &fake,
                &key,
                Some(&cached),
                |_, _, _, _, _, _| panic!("no projection after budget refusal"),
                &mut stage,
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(stage, AdapterDiagnostic::TargetBudget);
        assert_eq!(fake.preflight_reads.get(), 1);
        assert_eq!(fake.settings_writes.get(), 0);
        assert_eq!(fake.inventory_reads.get(), 0);
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn cache_preserves_evidence_window_and_expiry_but_recaptures_stale_or_invalid_evidence() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let fake = FullCapture::new();
        let original = fake.snapshot(&key, Duration::from_millis(100));
        let (raw, _, reused) = projected_targets(
            &fake,
            &key,
            Some(&original),
            fixture_projection,
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(raw.window, original.evidence.window);
        assert_eq!(reused.evidence.until(), original.evidence.until());
        assert_eq!(fake.source_reads.get(), 0);
        assert_eq!(fake.preflight_reads.get(), 0);
        assert!(accept_fixture_capture(raw).is_ok());
        for mode in 0..3 {
            let fake = FullCapture::new();
            let mut stale = fake.snapshot(&key, Duration::from_millis(100));
            match mode {
                0 => stale = fake.snapshot(&key, Duration::from_millis(CACHE_WINDOW_MS + 1)),
                1 => stale.evidence.window.started_unix_ms -= CACHE_WINDOW_MS,
                _ => stale.evidence.started = Instant::now() - AGE,
            }
            let (raw, _, fresh) = projected_targets(
                &fake,
                &key,
                Some(&stale),
                fixture_projection,
                &mut AdapterDiagnostic::TargetCapture,
            )
            .await
            .unwrap()
            .unwrap();
            assert_ne!(raw.window, stale.evidence.window);
            assert!(fresh.evidence.until() > stale.evidence.until());
            assert_eq!(fake.source_reads.get(), 1);
            assert_eq!(fake.preflight_reads.get(), 1);
            assert_eq!(fake.inventory_reads.get(), 1);
            assert_eq!(fake.inputs.get(), 0);
        }
        runner.await.unwrap();
    }

    fn text_fixture() -> (FullCapture, Bound) {
        let mut fake = FullCapture::new();
        fake.xml = XML.replace("XCUIElementTypeButton", "XCUIElementTypeTextField");
        let mut field = item();
        field["type"] = json!("XCUIElementTypeTextField");
        let mut keyboard = item();
        keyboard["ELEMENT"] = json!("keyboard");
        keyboard["element-6066-11e4-a52e-4f735466cecf"] = json!("keyboard");
        keyboard["type"] = json!("XCUIElementTypeKeyboard");
        keyboard["label"] = Value::Null;
        keyboard["attribute/name"] = Value::Null;
        keyboard["rect"] = json!({"x":0,"y":500,"width":400,"height":300});
        fake.xml=fake.xml.replace("</XCUIElementTypeApplication>","<XCUIElementTypeKeyboard visible='true' enabled='true' x='0' y='500' width='400' height='300'/></XCUIElementTypeApplication>");
        fake.items = json!([field, keyboard]);
        let mut bound = candidates(&fake.xml, &fake.items).unwrap().remove(0);
        bound.focused = Some(true);
        bound.public.can_type = true;
        bound.public.text_only = true;
        (fake, bound)
    }

    fn text_context_fixture(key: cache::Key) -> (FullCapture, Input, Snapshot) {
        let (mut fake, mut bound) = text_fixture();
        bound.public.text_only = true;
        bound.mapping = Some(fast::Mapping {
            viewport: [0., 0., 400., 800.],
            scale: 1.,
            display_id: None,
        });
        let snapshot = fake.snapshot(&key, Duration::ZERO);
        assert!(text::basis(&snapshot, &bound).is_some());
        let mut app = item();
        app["ELEMENT"] = json!("application");
        app["element-6066-11e4-a52e-4f735466cecf"] = json!("application");
        app["type"] = json!("XCUIElementTypeApplication");
        app["rect"] = json!({"x":0,"y":0,"width":400,"height":800});
        fake.items.as_array_mut().unwrap().push(app);
        let input = Input {
            token: 17,
            key,
            app: "example.app".into(),
            action: ElementAction::Text {
                target_id: bound.public.target_id.clone(),
                value: "fixture".into(),
            },
            bound,
            until: Instant::now() + AGE,
            text_basis: Some(snapshot.clone()),
        };
        (fake, input, snapshot)
    }

    #[tokio::test]
    async fn recent_full_text_context_uses_live_native_facts_without_source() {
        let runner = tokio::spawn(async {});
        let (fake, input, snapshot) = text_context_fixture(fixture_key(runner.id()));
        let result = rich_context(
            &fake,
            "session",
            ATTRS,
            text::revalidate(&fake, &input, &snapshot),
        )
        .await
        .unwrap();
        assert!(result.is_ok());
        assert_eq!(fake.source_reads.get(), 0);
        assert_eq!(fake.inventory_reads.get(), 1);
        assert_eq!(fake.image_reads.get(), 2);
        assert_eq!(fake.inputs.get(), 0);
        assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        for kind in [
            "Application",
            "Alert",
            "Sheet",
            "SecureTextField",
            "TextField",
            "TextView",
            "SearchField",
            "Keyboard",
        ] {
            assert!(
                fake.last_predicate
                    .borrow()
                    .contains(&format!("XCUIElementType{kind}"))
            );
        }
        assert!(fake.last_predicate.borrow().contains(r"\p{Cc}"));
    }

    #[tokio::test]
    async fn default_text_selects_narrow_or_full_proof_before_one_send() {
        let runner = tokio::spawn(async {});
        for narrow in [true, false] {
            let (fake, mut input, _) = text_context_fixture(fixture_key(runner.id()));
            if !narrow {
                input.text_basis = None;
            }
            let receipt = checked_dispatch(&fake, &input, Instant::now() + Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(receipt.accepted, Some(true));
            assert_eq!(receipt.action_kind, "text_element");
            assert_eq!(fake.source_reads.get() == 0, narrow);
            assert_eq!(fake.inputs.get(), 1);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        }
    }

    #[tokio::test]
    async fn fresh_text_contradictions_refuse_without_source_fallback() {
        let runner = tokio::spawn(async {});
        for mode in 0..16 {
            let (mut fake, input, _) = text_context_fixture(fixture_key(runner.id()));
            match mode {
                0 => fake.active_id = "different".into(),
                1 => fake.items[0]["attribute/value"] = json!("changed"),
                2 => fake.items[0]["attribute/selected"] = json!(true),
                3 => fake.items[0]["attribute/focused"] = json!(true),
                4 => fake.items[0]["rect"]["x"] = json!(11),
                5 => fake.items[0]["displayed"] = json!(false),
                6 => fake.items[0]["enabled"] = json!(false),
                7 => {
                    fake.items.as_array_mut().unwrap().remove(1);
                }
                8..=10 => {
                    let mut unsafe_row = item();
                    unsafe_row["ELEMENT"] = json!("unsafe");
                    unsafe_row["element-6066-11e4-a52e-4f735466cecf"] = json!("unsafe");
                    unsafe_row["type"] = json!(match mode {
                        8 => "XCUIElementTypeSecureTextField",
                        9 => "XCUIElementTypeAlert",
                        _ => "XCUIElementTypeSheet",
                    });
                    unsafe_row["displayed"] = json!(false);
                    fake.items.as_array_mut().unwrap().push(unsafe_row);
                }
                11 => fake.items[2]["rect"]["width"] = json!(399),
                12 => fake.items[0]["label"] = json!("Enter\u{0001} Passcode"),
                13 => fake.app = "unexpected.app",
                14 => fake.hittable = false,
                _ => {
                    fake.items[0]
                        .as_object_mut()
                        .unwrap()
                        .remove("attribute/selected");
                }
            }
            let result =
                checked_dispatch(&fake, &input, Instant::now() + Duration::from_secs(5)).await;
            assert!(result.is_err(), "mode {mode}");
            assert_eq!(fake.source_reads.get(), 0);
            assert_eq!(fake.inputs.get(), 0);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        }
    }

    #[tokio::test]
    async fn ordinary_text_region_changes_and_restoration_failure_never_send() {
        let runner = tokio::spawn(async {});
        for mode in 0..4 {
            let (mut fake, mut input, _) = text_context_fixture(fixture_key(runner.id()));
            input.bound.public.text_only = false;
            input.bound.region = Some(
                super::super::targets_pixels::Pixels::decode(&fake.image.bytes)
                    .unwrap()
                    .region([0., 0., 400., 800.], &input.bound.attrs, &mut 0)
                    .unwrap(),
            );
            if mode == 1 {
                fake.changed_second_image = true;
            } else if mode == 2 {
                let mut bytes = Vec::new();
                let mut pixels = vec![255; 400 * 800];
                pixels[25 * 400 + 25] = 0;
                let mut encoder = png::Encoder::new(&mut bytes, 400, 800);
                encoder.set_color(png::ColorType::Grayscale);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&pixels)
                    .unwrap();
                fake.image.bytes = bytes;
            } else if mode == 3 {
                fake.restoration_fails = true;
            }
            let result =
                checked_dispatch(&fake, &input, Instant::now() + Duration::from_secs(5)).await;
            assert_eq!(result.is_ok(), mode == 0);
            assert_eq!(fake.inputs.get(), usize::from(mode == 0));
            assert_eq!(fake.source_reads.get(), 0);
        }
    }

    #[tokio::test]
    async fn expired_text_context_never_gets_a_new_capture_timestamp() {
        let runner = tokio::spawn(async {});
        let (fake, input, mut snapshot) = text_context_fixture(fixture_key(runner.id()));
        snapshot.evidence.started -= Duration::from_secs(6);
        snapshot.evidence.finished -= Duration::from_secs(6);
        assert!(text::basis(&snapshot, &input.bound).is_none());
        assert!(text::revalidate(&fake, &input, &snapshot).await.is_err());
        assert_eq!(fake.inventory_reads.get(), 0);
        assert_eq!(fake.source_reads.get(), 0);
    }

    #[tokio::test]
    async fn text_only_mapping_changes_refuse_before_dispatch() {
        let runner = tokio::spawn(async {});
        for mode in 0..4 {
            let (mut fake, mut input, _) = text_context_fixture(fixture_key(runner.id()));
            assert!(input.bound.public.text_only);
            match mode {
                0 => {
                    fake.display_change_after_images = Some(2);
                    input.bound.mapping.as_mut().unwrap().display_id = Some(1);
                    input.text_basis.as_mut().unwrap().display_id = Some(1);
                }
                1 => fake.scale = 2.,
                2 => fake.window_size[0] = 399.,
                _ => {
                    let mut bytes = Vec::new();
                    let mut encoder = png::Encoder::new(&mut bytes, 399, 800);
                    encoder.set_color(png::ColorType::Grayscale);
                    encoder.set_depth(png::BitDepth::Eight);
                    encoder
                        .write_header()
                        .unwrap()
                        .write_image_data(&vec![255; 399 * 800])
                        .unwrap();
                    fake.image.bytes = bytes;
                }
            }
            assert!(
                checked_dispatch(&fake, &input, Instant::now() + Duration::from_secs(5))
                    .await
                    .is_err()
            );
            assert_eq!(fake.inputs.get(), 0);
            assert_eq!(fake.source_reads.get(), 0);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
            if mode == 0 {
                assert_eq!(fake.image_reads.get(), 2);
            }
        }
    }

    #[test]
    fn changed_field_pixels_keep_only_the_active_nonsecure_text_capability() {
        let (fake, mut bound) = text_fixture();
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 400, 800);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut pixels = vec![255; 400 * 800];
            pixels[25 * 400 + 20] = 0;
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
        let changed = ScreenImage {
            format: ImageFormat::Png,
            bytes,
        };
        for focused in [false, true] {
            bound.focused = Some(focused);
            bound.public.text_only = false;
            let mut entries = vec![bound.clone()];
            bind_regions(&fake.xml, &mut entries, &fake.image, &changed).unwrap();
            assert_eq!(entries.len(), usize::from(focused));
            if focused {
                assert!(entries[0].public.text_only);
            }
        }
        assert!(
            bind_regions(
                &fake.xml,
                &mut vec![bound],
                &fake.image,
                &ScreenImage {
                    format: ImageFormat::Png,
                    bytes: vec![1]
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn text_only_handle_never_taps_and_refuses_changed_identity_context_or_focus() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let (fake, bound) = text_fixture();
        let action = ElementAction::Text {
            target_id: bound.public.target_id.clone(),
            value: "fixture".into(),
        };
        let tap = ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        };
        assert!(
            revalidate(&fake, &key, "example.app", &bound, &tap)
                .await
                .is_err()
        );
        assert!(
            revalidate(&fake, &key, "example.app", &bound, &action)
                .await
                .is_ok()
        );
        for mode in 0..7 {
            let (mut fake, bound) = text_fixture();
            match mode {
                0 => fake.active_id = "other".into(),
                1 => fake.items[0]["attribute/value"] = json!("changed"),
                2 => fake.items[0]["rect"]["x"] = json!(11),
                3 => fake.items[0]["displayed"] = json!(false),
                4 => fake.items[0]["type"] = json!("XCUIElementTypeSecureTextField"),
                5 => {
                    fake.xml = fake.xml.replace(
                        "</AppiumAUT>",
                        "<XCUIElementTypeAlert visible='true'/></AppiumAUT>",
                    )
                }
                _ => fake.items[0]["enabled"] = json!(false),
            }
            assert!(
                revalidate(&fake, &key, "example.app", &bound, &action)
                    .await
                    .is_err(),
                "mode {mode}"
            );
            assert_eq!(fake.inputs.get(), 0);
        }
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn unequal_original_bracket_cannot_resurrect_cached_rows() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let fake = FullCapture::new();
        let mut original = fake.snapshot(&key, Duration::from_millis(10));
        original.bracket_equal = false;
        let (_, _, fresh) = projected_targets(
            &fake,
            &key,
            Some(&original),
            fixture_projection,
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inventory_reads.get(), 1);
        assert!(fresh.bracket_equal);
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn cache_validation_drift_reads_fresh_rows_once_without_input() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let mut fake = FullCapture::new();
        fake.changed_second_image = true;
        let mut original = fake.snapshot(&key, Duration::from_millis(10));
        original.xml = original.xml.replace("Choose", "Stale");
        let (raw, entries, fresh) = projected_targets(
            &fake,
            &key,
            Some(&original),
            fixture_projection,
            &mut AdapterDiagnostic::TargetCapture,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(raw.accessibility_tree.contains("Choose"));
        assert!(!raw.accessibility_tree.contains("Stale"));
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inventory_reads.get(), 1);
        assert_eq!(fake.image_reads.get(), 3);
        assert!(!fresh.bracket_equal);
        assert!(
            entries.is_empty(),
            "the changing target must not be admitted"
        );
        assert_eq!(fake.inputs.get(), 0);
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn post_capture_proof_requires_original_app_and_session() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        let fake = FullCapture::new();
        let input = fake.input(key.clone(), Instant::now() + AGE);
        let mut snapshot = fake.snapshot(&key, Duration::ZERO);
        assert!(!snapshot_effect(&input, &snapshot).unwrap());
        snapshot.items[0]["attribute/selected"] = json!(true);
        assert!(snapshot_effect(&input, &snapshot).unwrap());
        snapshot.app = "other.app".into();
        assert!(snapshot_effect(&input, &snapshot).is_err());
        snapshot.app = input.app.clone();
        snapshot.key.session = "other-session".into();
        assert!(snapshot_effect(&input, &snapshot).is_err());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn cache_aging_during_reads_or_projection_gets_one_fresh_capture() {
        let runner = tokio::spawn(async {});
        let key = fixture_key(runner.id());
        for mode in 0..2 {
            let mut fake = FullCapture::new();
            let delay = Duration::from_millis(600);
            if mode == 0 {
                fake.second_image_delay = delay;
            }
            let original = fake.snapshot(&key, Duration::from_millis(2_500));
            let projections = Cell::new(0);
            let (raw, _, fresh) = projected_targets(
                &fake,
                &key,
                Some(&original),
                |identity, xml, before, after, scale, entries| {
                    projections.set(projections.get() + 1);
                    if mode == 1 && projections.get() == 1 {
                        std::thread::sleep(delay);
                    }
                    fixture_projection(identity, xml, before, after, scale, entries)
                },
                &mut AdapterDiagnostic::TargetCapture,
            )
            .await
            .unwrap()
            .unwrap();
            assert_ne!(raw.window, original.evidence.window);
            assert!(fresh.evidence.until() > original.evidence.until());
            assert!(accept_fixture_capture(raw).is_ok());
            assert_eq!(fake.source_reads.get(), 1);
            assert_eq!(fake.inventory_reads.get(), 1);
            assert_eq!(fake.image_reads.get(), if mode == 0 { 3 } else { 4 });
            assert_eq!(projections.get(), if mode == 0 { 1 } else { 2 });
            assert_eq!(fake.inputs.get(), 0);
            assert_eq!(*fake.settings.borrow(), FullCapture::original_settings());
        }
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn restoration_failure_is_not_hidden_by_a_valid_capture_window() {
        let runner = tokio::spawn(async {});
        let mut fake = FullCapture::new();
        fake.restoration_fails = true;
        assert!(
            projected_targets(
                &fake,
                &fixture_key(runner.id()),
                None,
                fixture_projection,
                &mut AdapterDiagnostic::TargetCapture,
            )
            .await
            .is_err()
        );
        assert_eq!(fake.source_reads.get(), 1);
        assert_eq!(fake.inputs.get(), 0);
        assert_ne!(*fake.settings.borrow(), FullCapture::original_settings());
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn session_generation_app_revision_and_expiry_are_binding() {
        let runner = tokio::spawn(async {});
        let other = tokio::spawn(async {});
        let key = cache::Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "session".into(),
            runner: runner.id(),
        };
        let now = Instant::now();
        let targets = Targets {
            key: key.clone(),
            revision: Some(Revision(7)),
            app: "example.app".into(),
            until: now + AGE,
            entries: vec![],
        };
        assert!(targets.context_matches(Some(&key), Revision(7), Some("example.app"), now));
        let mut wrong = key.clone();
        wrong.runner = other.id();
        assert!(!targets.context_matches(Some(&wrong), Revision(7), Some("example.app"), now));
        wrong = key.clone();
        wrong.session = "changed".into();
        assert!(!targets.context_matches(Some(&wrong), Revision(7), Some("example.app"), now));
        wrong = key.clone();
        wrong.device = HardwareId::new("other").unwrap();
        assert!(!targets.context_matches(Some(&wrong), Revision(7), Some("example.app"), now));
        assert!(!targets.context_matches(Some(&key), Revision(8), Some("example.app"), now));
        assert!(!targets.context_matches(Some(&key), Revision(7), Some("other.app"), now));
        assert!(!targets.context_matches(Some(&key), Revision(7), Some("example.app"), now + AGE));
        runner.await.unwrap();
        other.await.unwrap();
    }

    #[test]
    fn opaque_handles_reject_duplicate_native_ids_and_ambiguous_xml() {
        let a = item();
        let bound = candidates(XML, &json!([a])).unwrap();
        assert!(bound[0].public.target_id.starts_with("t_"));
        assert_ne!(bound[0].public.target_id, "native-one");
        assert!(candidates(XML, &json!([item(), item()])).is_err());
        let mut alias = item();
        alias["ELEMENT"] = json!("native-two");
        alias["element-6066-11e4-a52e-4f735466cecf"] = json!("native-two");
        assert!(candidates(XML, &json!([item(), alias])).is_err());
        let node = XML
            .split("<XCUIElementTypeButton")
            .nth(1)
            .unwrap()
            .split("/>")
            .next()
            .unwrap();
        let duplicate = XML.replace(
            "</XCUIElementTypeApplication>",
            &format!("<XCUIElementTypeButton{node}/></XCUIElementTypeApplication>"),
        );
        assert!(candidates(&duplicate, &json!([item()])).unwrap().is_empty());
    }
    #[test]
    fn remapping_and_target_change_refuse_but_unrelated_animation_does_not() {
        let original = candidates(XML, &json!([item()])).unwrap().remove(0);
        let animated = XML.replace(
            "</XCUIElementTypeApplication>",
            "<XCUIElementTypeOther label=\"frame 2\"/></XCUIElementTypeApplication>",
        );
        assert!(same_target(
            &original,
            &candidates(&animated, &json!([item()])).unwrap()
        ));
        let mut remapped = item();
        remapped["ELEMENT"] = json!("another");
        remapped["element-6066-11e4-a52e-4f735466cecf"] = json!("another");
        assert!(!same_target(
            &original,
            &candidates(XML, &json!([remapped])).unwrap()
        ));
        let mut changed = item();
        changed["label"] = json!("New");
        assert!(!same_target(
            &original,
            &candidates(&XML.replace("Choose", "New"), &json!([changed])).unwrap()
        ));
    }
    #[test]
    fn animated_candidate_is_omitted_without_discarding_stable_targets() {
        let mut nodes = candidates(XML, &json!([item()])).unwrap();
        let mut second = nodes[0].clone();
        second.attrs.insert("x".into(), "120".into());
        second.public.target_id.push('x');
        nodes.push(second);
        let make = |changed: bool| {
            let mut pixels = vec![255; 400 * 800 * 4];
            if changed {
                pixels[(20 * 400 + 120) * 4] = 0;
            }
            let mut bytes = Vec::new();
            {
                let mut e = png::Encoder::new(&mut bytes, 400, 800);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                e.write_header().unwrap().write_image_data(&pixels).unwrap();
            }
            ScreenImage {
                format: ImageFormat::Png,
                bytes,
            }
        };
        bind_regions(XML, &mut nodes, &make(false), &make(true)).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].attrs.get("x").unwrap(), "10");
        let mut malformed = nodes;
        malformed[0].attrs.insert("x".into(), "-1".into());
        assert!(bind_regions(XML, &mut malformed, &make(false), &make(false)).is_err());
    }

    #[test]
    fn tap_effect_excludes_animation_and_id_remap_but_accepts_explicit_changes() {
        let bound = candidates(XML, &json!([item()])).unwrap().remove(0);
        assert!(!tap_effect(&bound, XML, &json!([]), None).unwrap());
        let mut invalid = item();
        invalid["displayed"] = json!(false);
        invalid["enabled"] = Value::Null;
        assert!(tap_effect(&bound, XML, &json!([invalid]), None).is_err());
        assert!(!tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([item()]), None).unwrap());
        let mut remapped = item();
        remapped["ELEMENT"] = json!("other");
        remapped["element-6066-11e4-a52e-4f735466cecf"] = json!("other");
        assert!(!tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([remapped]), None).unwrap());
        assert!(tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([]), None).unwrap());
        let mut representation = item();
        representation["attribute/selected"] = json!("false");
        assert!(!tap_effect(&bound, XML, &json!([representation]), None).unwrap());
        let mut selected = item();
        selected["attribute/selected"] = json!(true);
        assert!(tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([selected]), None).unwrap());
        let mut hidden = item();
        hidden["displayed"] = json!(false);
        assert!(tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([hidden]), None).unwrap());
        let mut malformed = item();
        malformed.as_object_mut().unwrap().remove("attribute/value");
        assert!(tap_effect(&bound, "<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>", &json!([malformed]), None).is_err());
    }

    #[test]
    fn incomplete_hidden_disabled_secure_and_outside_targets_fail_closed() {
        for field in [
            "label",
            "attribute/name",
            "attribute/value",
            "attribute/placeholderValue",
            "attribute/focused",
            "rect",
        ] {
            let mut bad = item();
            bad.as_object_mut().unwrap().remove(field);
            assert!(candidates(XML, &json!([bad])).is_err(), "{field}");
        }
        for field in ["enabled", "displayed"] {
            let mut bad = item();
            bad[field] = json!(false);
            assert!(candidates(XML, &json!([bad])).unwrap().is_empty());
        }
        for suffix in [
            "<XCUIElementTypeSecureTextField/>",
            "<XCUIElementTypeAlert/>",
            "<XCUIElementTypeOther label=\"Enter Passcode\"/>",
        ] {
            assert!(
                candidates(
                    &XML.replace(
                        "</XCUIElementTypeApplication>",
                        &format!("{suffix}</XCUIElementTypeApplication>")
                    ),
                    &json!([item()])
                )
                .is_err()
            );
        }
        let mut outside = item();
        outside["rect"]["x"] = json!(390);
        assert!(
            candidates(&XML.replace("x=\"10\"", "x=\"390\""), &json!([outside]))
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn inventory_includes_cells_text_and_images_without_visibility_filtering() {
        struct Inventory;
        impl WdaRequest for Inventory {
            async fn request(
                &self,
                method: &str,
                path: &str,
                body: &[u8],
                _: Duration,
            ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
                assert_eq!((method, path), ("POST", "/session/session/elements"));
                let body: Value = serde_json::from_slice(body).unwrap();
                let predicate = body["value"].as_str().unwrap();
                assert!(!predicate.contains("enabled"));
                assert!(!predicate.contains("visible"));
                let rows: Vec<_> = [
                    "XCUIElementTypeCell",
                    "XCUIElementTypeStaticText",
                    "XCUIElementTypeImage",
                ]
                .into_iter()
                .filter(|kind| predicate.contains(&format!("'{kind}'")))
                .map(|kind| json!({"type":kind}))
                .collect();
                Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&json!({"sessionId":"session","value":rows})).unwrap(),
                })
            }
        }
        assert_eq!(
            query(&Inventory, "session")
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    struct Fake {
        mode: u8,
        sends: Cell<usize>,
    }
    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let result = if method == "POST" {
                assert_eq!(path, "/session/session/wda/keys");
                assert_eq!(
                    serde_json::from_slice::<Value>(body).unwrap()["value"],
                    json!(["hello"])
                );
                self.sends.set(self.sends.get() + 1);
                if self.mode == 4 {
                    return Err(crate::wda_transport::WdaError::AfterSent);
                }
                Value::Null
            } else if path.ends_with("/element/active") {
                json!({"ELEMENT":if self.mode==1{"other"}else{"field"}})
            } else if path.ends_with("/wda/activeAppInfo") {
                json!({"bundleId":if self.mode==2{"com.apple.springboard"}else{"example.editor"}})
            } else {
                assert!(path.ends_with("/wda/locked"));
                json!(self.mode == 3)
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":result})).unwrap(),
            })
        }
    }
    #[tokio::test]
    async fn final_focus_app_and_lock_checks_precede_one_native_text_dispatch() {
        for mode in 0..=4 {
            let fake = Fake {
                mode,
                sends: Cell::new(0),
            };
            let action = ElementAction::Text {
                target_id: "t_0123456789abcdef0123456789abcdef".into(),
                value: "hello".into(),
            };
            let result =
                dispatch_native(&fake, "session", "example.editor", "field", &action).await;
            assert_eq!(result.is_ok(), mode == 0);
            assert_eq!(fake.sends.get(), usize::from(mode == 0 || mode == 4));
        }
    }
}
