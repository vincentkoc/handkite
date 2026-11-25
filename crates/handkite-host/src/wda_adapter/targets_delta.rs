//! Incremental native presentation. This is never a complete XML hierarchy.
use super::super::targets_pixels::Pixels;
use super::*;

#[derive(Clone)]
pub(in crate::wda_adapter) struct DeltaBaseline {
    pub(super) revision: Option<Revision>,
    key: cache::Key,
    app: String,
    mapping: fast::Mapping,
    image: ScreenImage,
    entries: Vec<(Bound, Instant)>,
    dirty: Vec<[f64; 4]>,
    until: Instant,
    /// Original complete capture time; bounds immutable snapshot replay.
    captured: Instant,
    /// Only an untouched complete snapshot can use immutable replay.
    snapshot_replay: bool,
}

impl DeltaBaseline {
    pub(super) fn from_full(
        revision: Revision,
        snapshot: &Snapshot,
        entries: &[Bound],
    ) -> Result<Self, AdapterError> {
        Ok(Self {
            revision: Some(revision),
            key: snapshot.key.clone(),
            app: snapshot.app.clone(),
            mapping: fast::Mapping {
                viewport: super::super::targets_pixels::viewport(&snapshot.xml)?,
                scale: snapshot.scale,
                display_id: snapshot.display_id,
            },
            image: snapshot.image.clone(),
            entries: entries
                .iter()
                .cloned()
                .map(|e| (e, snapshot.evidence.until()))
                .collect(),
            dirty: Vec::new(),
            until: snapshot.evidence.until(),
            captured: snapshot.evidence.started,
            snapshot_replay: true,
        })
    }

    fn can_replay_snapshot(
        &self,
        snapshot: &Snapshot,
        key: &cache::Key,
        revision: Revision,
        pending_effect: bool,
    ) -> bool {
        self.snapshot_replay
            && !pending_effect
            && self.dirty.is_empty()
            && self.revision == Some(revision)
            && self.key == *key
            && snapshot.key == *key
            && self.app == snapshot.app
            && self.mapping.display_id == snapshot.display_id
            && self.mapping.scale == snapshot.scale
            && self.image == snapshot.image
            && self.captured == snapshot.evidence.started
            && self.until == snapshot.evidence.until()
            && snapshot.bracket_equal
            && snapshot.cost.within_visibility_budget()
            && snapshot.evidence.actionable().unwrap_or(false)
    }

    fn input_deadline(&self) -> Instant {
        self.entries
            .iter()
            .map(|(_, until)| *until)
            .min()
            .unwrap_or(self.until)
            .min(self.until)
    }

    pub(super) fn consume(&mut self, target: &ElementTarget) {
        self.snapshot_replay = false;
        self.entries
            .retain(|(e, _)| e.public.target_id != target.target_id);
        self.dirty.push(target.rect);
    }
}

pub(super) fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}
pub(super) fn contains(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] <= b[0] && a[1] <= b[1] && a[0] + a[2] >= b[0] + b[2] && a[1] + a[3] >= b[1] + b[3]
}
pub(super) fn changes(
    a: &Pixels,
    b: &Pixels,
    mapping: fast::Mapping,
) -> Result<Option<[f64; 4]>, AdapterError> {
    if a.width != b.width || a.height != b.height {
        return Err(AdapterError);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (a.width, a.height, 0, 0);
    for (i, (left, right)) in a
        .rgba
        .chunks_exact(4)
        .zip(b.rgba.chunks_exact(4))
        .enumerate()
    {
        if left != right {
            let x = i as u32 % a.width;
            let y = i as u32 / a.width;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    if x1 == 0 {
        return Ok(None);
    }
    // Include the same one-pixel raster uncertainty as target-region admission.
    let sx = a.width as f64 / mapping.viewport[2];
    let sy = a.height as f64 / mapping.viewport[3];
    x0 = x0.saturating_sub(1);
    y0 = y0.saturating_sub(1);
    x1 = (x1 + 1).min(a.width);
    y1 = (y1 + 1).min(a.height);
    Ok(Some([
        x0 as f64 / sx,
        y0 as f64 / sy,
        (x1 - x0) as f64 / sx,
        (y1 - y0) as f64 / sy,
    ]))
}
fn field(kind: &str) -> bool {
    matches!(
        kind,
        "XCUIElementTypeTextField"
            | "XCUIElementTypeTextView"
            | "XCUIElementTypeSearchField"
            | "XCUIElementTypeKeyboard"
    )
}
fn predicate(rects: &[[f64; 4]], acted: Option<&Bound>) -> Result<String, AdapterError> {
    // WDA does not rewrite key paths nested inside arithmetic expressions.
    let mut areas = Vec::new();
    for &[x, y, w, h] in rects {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || w <= 0. || h <= 0. {
            return Err(AdapterError);
        }
        areas.push(format!("(wdRect.x < {} AND wdRect.y < {} AND wdRect.x + wdRect.width > {x} AND wdRect.y + wdRect.height > {y})",x+w,y+h));
    }
    let area = if areas.is_empty() {
        "FALSEPREDICATE".into()
    } else {
        areas.join(" OR ")
    };
    // A pending tap's exact descriptor is enumerated anywhere, as in its
    // standalone effect sample, so duplicates and moved rows stay visible.
    let acted = match acted {
        Some(bound) => format!(
            "({}) OR ",
            input_predicate(
                bound,
                &ElementAction::Tap {
                    target_id: bound.public.target_id.clone(),
                },
            )?
        ),
        None => String::new(),
    };
    Ok(format!(
        "{acted}({}) OR (type IN {{'XCUIElementTypeTextField','XCUIElementTypeTextView','XCUIElementTypeSearchField','XCUIElementTypeKeyboard'}}) OR (({area}) AND type IN {{'XCUIElementTypeButton','XCUIElementTypeMenuItem','XCUIElementTypeCell','XCUIElementTypeStaticText','XCUIElementTypeLink','XCUIElementTypeImage','XCUIElementTypePickerWheel'}})",
        fast::context_predicate()?
    ))
}
fn new_bound(row: &Value, mapping: fast::Mapping) -> Result<Option<Bound>, AdapterError> {
    let attrs = fast::attrs(row)?;
    let kind = attrs["type"].as_str();
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
    ) || attrs["enabled"] != "true"
        || attrs["visible"] != "true"
    {
        return Ok(None);
    }
    let rect = ["x", "y", "width", "height"].map(|k| attrs[k].parse::<f64>().unwrap());
    if rect[0] < 0.
        || rect[1] < 0.
        || rect[2] <= 0.
        || rect[3] <= 0.
        || rect[0] + rect[2] > mapping.viewport[2]
        || rect[1] + rect[3] > mapping.viewport[3]
    {
        return Ok(None);
    }
    let mut random = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
        .map_err(|_| AdapterError)?;
    Ok(Some(Bound {
        public: ElementTarget {
            target_id: format!(
                "t_{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            source: "native".into(),
            kind: kind.into(),
            label: attrs.get("label").cloned().unwrap_or_default(),
            can_type: false,
            text_only: false,
            rect,
            value: attrs.get("value").map(|s| s.chars().take(256).collect()),
            value_truncated: attrs.get("value").is_some_and(|s| s.chars().count() > 256),
        },
        native: id(row)?,
        attrs,
        rich: row.clone(),
        region: None,
        focused: row["attribute/focused"].as_bool(),
        ocr: None,
        ocr_before: None,
        mapping: Some(mapping),
    }))
}
fn presentation(mapping: fast::Mapping, app: &str, entries: &[Bound]) -> String {
    let esc = |s: &str| quick_xml::escape::escape(s).to_string();
    let mut xml = format!(
        "<AppiumAUT source=\"native_regions\"><XCUIElementTypeApplication type=\"XCUIElementTypeApplication\" name=\"{}\" visible=\"true\" enabled=\"true\" x=\"0\" y=\"0\" width=\"{}\" height=\"{}\">",
        esc(app),
        mapping.viewport[2],
        mapping.viewport[3]
    );
    for e in entries {
        xml.push('<');
        xml.push_str(&e.public.kind);
        for (k, v) in &e.attrs {
            xml.push_str(&format!(" {}=\"{}\"", k, esc(v)));
        }
        xml.push_str(&format!(
            " source=\"{}\" target_id=\"{}\"/>",
            esc(&e.public.source),
            e.public.target_id
        ));
    }
    xml.push_str("</XCUIElementTypeApplication></AppiumAUT>");
    xml
}

fn refreshed(
    base: &DeltaBaseline,
    rows: &Value,
    before: &Pixels,
    after: &Pixels,
    areas: &[[f64; 4]],
) -> Result<Vec<(Bound, Instant)>, AdapterError> {
    let checked = fast::checked_rows(rows, base.mapping).inspect_err(|_| {
        crate::timing::outcome("target_delta_rows_valid", false);
    })?;
    crate::timing::outcome("target_delta_rows_valid", true);
    // Editable fields are always enumerated in full, so caret drift inside one
    // is covered by fresh rows. Any other drift outside the queried areas refuses.
    let fields = checked
        .iter()
        .filter(|row| row["type"].as_str().is_some_and(field))
        .filter_map(|row| {
            let r = &row["rect"];
            Some([
                r["x"].as_f64()?,
                r["y"].as_f64()?,
                r["width"].as_f64()?,
                r["height"].as_f64()?,
            ])
        })
        .collect::<Vec<_>>();
    if let Some(changed) = changes(before, after, base.mapping)?
        && !areas
            .iter()
            .chain(&fields)
            .any(|area| contains(*area, changed))
    {
        crate::timing::outcome("target_delta_changes_covered", false);
        return Err(AdapterError);
    }
    crate::timing::outcome("target_delta_changes_covered", true);
    let rows = fast::checked_rows(rows, base.mapping)?;
    let now = Instant::now();
    let mut result = Vec::new();
    let mut first_budget = 0;
    let mut last_budget = 0;
    // Pixels preserve presentation, not semantic admission or original expiry.
    for (old, until) in &base.entries {
        if old.ocr.is_some()
            || now >= *until
            || field(&old.public.kind)
            || areas.iter().any(|area| intersects(*area, old.public.rect))
            || rows
                .iter()
                .any(|r| id(r).ok().as_deref() == Some(&old.native))
        {
            continue;
        }
        let first = before.region(base.mapping.viewport, &old.attrs, &mut first_budget)?;
        let last = after.region(base.mapping.viewport, &old.attrs, &mut last_budget)?;
        if old.region.as_ref() == Some(&first) && first == last {
            let mut retained = old.clone();
            retained.public.source = "native_cached".into();
            result.push((retained, *until));
        }
    }
    for row in &rows {
        let Some(mut bound) = new_bound(row, base.mapping)? else {
            continue;
        };
        if rows
            .iter()
            .filter(|r| descriptor_matches(r, &bound.attrs))
            .count()
            != 1
        {
            continue;
        }
        let first = before.region(base.mapping.viewport, &bound.attrs, &mut first_budget)?;
        let last = after.region(base.mapping.viewport, &bound.attrs, &mut last_budget)?;
        if first != last {
            continue;
        }
        bound.region = Some(last);
        if let Some((old, until)) = base.entries.iter().find(|(old, until)| {
            now < *until
                && old.native == bound.native
                && old.rich == bound.rich
                && old.region == bound.region
        }) {
            bound.public.target_id = old.public.target_id.clone();
            result.push((bound, *until));
        } else {
            result.push((bound, now + AGE));
        }
    }
    Ok(result)
}

/// Bounded image-only polls before an incremental bracket (about 1.2 s).
const SETTLE_FRAMES: usize = 10;
const CARET_WIDTH: f64 = 6.;
const CARET_HEIGHT: f64 = 80.;

struct DeltaCapture {
    raw: RawObservation,
    entries: Vec<(Bound, Instant)>,
    /// Effect sample for a pending tap and the WDA cache entries it may add.
    effect: Option<(fast::Outcome, usize)>,
}

async fn capture(
    wda: &impl WdaRequest,
    base: &DeltaBaseline,
    acted: Option<&Bound>,
) -> Result<DeltaCapture, AdapterError> {
    let _span = crate::timing::Span::new("target_delta_capture");
    let started = unix_ms()?;
    super::super::display::verify_selection(wda, &base.key.session, base.mapping.display_id)
        .await?;
    let (app, before) = tokio::join!(
        app_matches(wda, &base.key.session, &base.app),
        screenshot(wda)
    );
    app?;
    let before = before?;
    let mut first = Pixels::decode(&before.bytes)?;
    first.validate_mapping(
        base.mapping.viewport,
        base.mapping.scale,
        [base.mapping.viewport[2], base.mapping.viewport[3]],
    )?;
    // An observation requested right after input can start before the
    // transition does. Begin the bracket at the first of two identical frames;
    // an unsettled screen keeps the last frame and the bracket check decides.
    for _ in 0..SETTLE_FRAMES {
        let next = screenshot(wda).await?;
        let pixels = Pixels::decode(&next.bytes)?;
        // A caret-sized bar is not a transition; the bracket still checks it.
        let settled = changes(&first, &pixels, base.mapping)?
            .is_none_or(|[_, _, w, h]| w <= CARET_WIDTH && h <= CARET_HEIGHT);
        first = pixels;
        if settled {
            break;
        }
    }
    let old = Pixels::decode(&base.image.bytes)?;
    let mut areas = base.dirty.clone();
    if let Some(area) = changes(&old, &first, base.mapping)? {
        areas.push(area);
    }
    let focus = acted.is_some_and(fast::focus_target);
    let filter = predicate(&areas, acted)?;
    let (rows, focused) = tokio::join!(query_predicate(wda, &base.key.session, &filter), async {
        if focus {
            active(wda, &base.key.session).await.map(Some)
        } else {
            Ok(None)
        }
    });
    let (rows, focused) = (rows?, focused?);
    let after = screenshot(wda).await?;
    let last = Pixels::decode(&after.bytes)?;
    last.validate_mapping(
        base.mapping.viewport,
        base.mapping.scale,
        [base.mapping.viewport[2], base.mapping.viewport[3]],
    )?;
    let entries = refreshed(base, &rows, &first, &last, &areas)?;
    let effect = match acted {
        Some(bound) => {
            let checked = fast::checked_rows(&rows, base.mapping)?;
            // The standalone sample queried only the acted descriptor plus
            // application context; classify exactly that row set.
            let sample = checked
                .iter()
                .filter(|row| {
                    descriptor_matches(row, &bound.attrs)
                        || row["type"] == "XCUIElementTypeApplication"
                })
                .cloned()
                .collect::<Vec<_>>();
            let region = last.region(base.mapping.viewport, &bound.attrs, &mut 0)?;
            Some((
                fast::classify(bound, &sample, &region, focused.as_deref()),
                checked.len() + usize::from(focus),
            ))
        }
        None => None,
    };
    app_matches(wda, &base.key.session, &base.app).await?;
    super::super::display::verify_selection(wda, &base.key.session, base.mapping.display_id)
        .await?;
    let public = entries.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>();
    let xml = presentation(base.mapping, &base.app, &public);
    let safe_context = crate::wda_ui_admission::safe_context(&xml);
    crate::timing::outcome("target_delta_source_safe", safe_context);
    if !safe_context {
        return Err(AdapterError);
    }
    Ok(DeltaCapture {
        raw: RawObservation {
            connected_usb: base.key.device.clone(),
            wda_target: base.key.device.clone(),
            window: ObservationWindow {
                started_unix_ms: started,
                finished_unix_ms: unix_ms()?,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some(base.app.clone()),
            },
            accessibility_tree: xml,
            screenshot: after,
        },
        entries,
        effect,
    })
}

impl WdaAdapter {
    pub(in crate::wda_adapter) fn capture_target_delta(
        &mut self,
        revision: Revision,
    ) -> Result<Option<(RawObservation, Vec<ElementTarget>)>, AdapterError> {
        // A pending tap's sample rides on this capture. Its stale-ID proof, if
        // needed, stays behind the cache eviction bound tracked by the proof.
        let Some(key) = self.cache_key() else {
            self.finish_pending_effect(true)?;
            return Ok(None);
        };
        if self
            .target_delta
            .as_ref()
            .zip(self.target_snapshot.as_ref())
            .is_some_and(|(base, snapshot)| {
                base.can_replay_snapshot(snapshot, &key, revision, self.pending_effect.is_some())
            })
        {
            // The ordinary full path proves current native/app/frame identity
            // and replays immutable data without a rich transaction. MCP still
            // computes a compact delta against the acknowledged presentation.
            crate::timing::outcome("target_delta_snapshot_route", true);
            return Ok(None);
        }
        let acted = self.pending_for_delta(&key)?;
        self.error_diagnostic = Some(AdapterDiagnostic::TargetCapture);
        self.validate_owned()?;
        let Some(mut base) = self.target_delta.take().filter(|b| {
            b.revision == Some(revision)
                && self.cache_key().as_ref() == Some(&b.key)
                && Instant::now() < b.until
        }) else {
            return Ok(None);
        };
        base.snapshot_replay = false;
        self.targets = None;
        self.target_snapshot = None;
        self.target_input = None;
        self.target_readback = None;
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let device = base.key.device.clone();
        let native_started = Instant::now();
        let (lock, result) =
            self.runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(capture_with_native(
                    move || native_lock::query(&device).state,
                    target_context(
                        &wda,
                        &base.key.session,
                        capture(&wda, &base, acted.as_ref().map(|input| &input.bound)),
                    ),
                ));
        self.cache
            .refresh_native(Some(base.key.clone()), native_started, || lock);
        let result = result.map_err(|e| {
            self.text_response_poisoned = true;
            self.error_diagnostic = Some(AdapterDiagnostic::TargetSettings);
            e
        })?;
        let DeltaCapture {
            raw,
            entries,
            effect,
        } = self.target_work(result)?;
        self.validate_owned()?;
        if lock != LockState::Unlocked || self.cache_key().as_ref() != Some(&base.key) {
            return Err(AdapterError);
        }
        if let Some((outcome, inserted)) = effect {
            self.record_delta_effect(
                outcome,
                inserted,
                RawImageObservation {
                    connected_usb: raw.connected_usb.clone(),
                    wda_target: raw.wda_target.clone(),
                    window: raw.window.clone(),
                    screenshot: raw.screenshot.clone(),
                },
                native_started,
            )?;
        }
        base.image = raw.screenshot.clone();
        base.entries = entries;
        base.dirty.clear();
        base.revision = None;
        base.until = Instant::now() + AGE;
        let until = base.input_deadline();
        let entries = base
            .entries
            .iter()
            .map(|(e, _)| e.clone())
            .collect::<Vec<_>>();
        let public = entries.iter().map(|e| e.public.clone()).collect();
        self.targets = Some(Targets {
            key: base.key.clone(),
            revision: None,
            app: base.app.clone(),
            until,
            entries,
        });
        self.target_delta = Some(base);
        Ok(Some((raw, public)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn region_predicate_uses_explicit_wda_paths_inside_arithmetic() {
        let p = predicate(&[[1., 2., 3., 4.]], None).unwrap();
        assert!(p.contains("wdRect.x + wdRect.width > 1"));
        assert!(p.contains("wdRect.y + wdRect.height > 2"));
        assert!(!p.contains("(rect."));
    }

    fn mapping() -> fast::Mapping {
        fast::Mapping {
            viewport: [0., 0., 20., 20.],
            scale: 1.,
            display_id: None,
        }
    }
    fn pixels() -> Pixels {
        Pixels {
            width: 20,
            height: 20,
            rgba: vec![255; 20 * 20 * 4],
        }
    }
    fn row(name: &str, kind: &str, rect: [f64; 4]) -> Value {
        json!({"ELEMENT":name.replace('.', "-"),"element-6066-11e4-a52e-4f735466cecf":name.replace('.', "-"),"type":kind,"label":name,"attribute/name":name,"attribute/value":null,"attribute/placeholderValue":null,"attribute/focused":false,"attribute/selected":false,"enabled":true,"displayed":true,"rect":{"x":rect[0],"y":rect[1],"width":rect[2],"height":rect[3]}})
    }
    fn app() -> Value {
        row(
            "example.app",
            "XCUIElementTypeApplication",
            [0., 0., 20., 20.],
        )
    }
    fn baseline() -> DeltaBaseline {
        let r = row("old", "XCUIElementTypeButton", [2., 2., 3., 3.]);
        let mut bound = new_bound(&r, mapping()).unwrap().unwrap();
        bound.region = Some(
            pixels()
                .region(mapping().viewport, &bound.attrs, &mut 0)
                .unwrap(),
        );
        DeltaBaseline {
            revision: Some(Revision(1)),
            key: cache::Key {
                device: HardwareId::new("fixture").unwrap(),
                session: "session".into(),
                runner: std::thread::spawn(|| {
                    tokio::runtime::Runtime::new().unwrap().spawn(async {}).id()
                })
                .join()
                .unwrap(),
            },
            app: "example.app".into(),
            mapping: mapping(),
            image: ScreenImage {
                format: handkite_core::protocol::ImageFormat::Png,
                bytes: vec![],
            },
            entries: vec![(bound, Instant::now() + AGE)],
            dirty: vec![],
            until: Instant::now() + AGE,
            captured: Instant::now(),
            snapshot_replay: true,
        }
    }

    fn complete_snapshot() -> (DeltaBaseline, Snapshot) {
        let base = baseline();
        let now = Instant::now();
        let now_ms = unix_ms().unwrap();
        let xml = "<AppiumAUT><XCUIElementTypeApplication name='example.app' visible='true' enabled='true' x='0' y='0' width='20' height='20'><XCUIElementTypeButton name='old' label='old' visible='true' enabled='true' x='2' y='2' width='3' height='3'/></XCUIElementTypeApplication></AppiumAUT>".to_owned();
        let snapshot = Snapshot {
            key: base.key.clone(),
            cost: super::super::super::outline::xml_cost(&xml).unwrap(),
            xml,
            items: json!([app(), base.entries[0].0.rich.clone()]),
            app: base.app.clone(),
            image: base.image.clone(),
            evidence: EvidenceWindow {
                window: ObservationWindow {
                    started_unix_ms: now_ms - 2200,
                    finished_unix_ms: now_ms - 2100,
                },
                started: now - Duration::from_millis(2200),
                finished: now - Duration::from_millis(2100),
            },
            scale: base.mapping.scale,
            display_id: base.mapping.display_id,
            focused: None,
            bracket_equal: true,
        };
        let entries = base
            .entries
            .iter()
            .map(|(bound, _)| bound.clone())
            .collect::<Vec<_>>();
        let base = DeltaBaseline::from_full(Revision(1), &snapshot, &entries).unwrap();
        (base, snapshot)
    }

    #[test]
    fn immutable_delta_fallback_preserves_original_capture_time_and_expiry() {
        let (base, snapshot) = complete_snapshot();
        assert!(base.can_replay_snapshot(&snapshot, &base.key, Revision(1), false));
        let entries = base
            .entries
            .iter()
            .map(|(bound, _)| bound.clone())
            .collect::<Vec<_>>();
        let next = DeltaBaseline::from_full(Revision(2), &snapshot, &entries).unwrap();
        assert_eq!(next.captured, base.captured);
        assert_eq!(next.until, base.until);
        assert_eq!(next.entries[0].1, base.entries[0].1);
    }

    #[test]
    fn consumed_or_partial_delta_cannot_revive_snapshot_replay() {
        let (mut base, snapshot) = complete_snapshot();
        let target = base.entries[0].0.public.clone();
        base.consume(&target);
        base.dirty.clear();
        assert!(!base.can_replay_snapshot(&snapshot, &base.key, Revision(1), false));
        let (mut partial, snapshot) = complete_snapshot();
        partial.snapshot_replay = false;
        assert!(!partial.can_replay_snapshot(&snapshot, &partial.key, Revision(1), false));
    }

    #[test]
    fn delta_snapshot_route_refuses_pending_or_changed_context() {
        let (base, snapshot) = complete_snapshot();
        assert!(!base.can_replay_snapshot(&snapshot, &base.key, Revision(1), true));
        assert!(!base.can_replay_snapshot(&snapshot, &base.key, Revision(2), false));
        let mut other_key = base.key.clone();
        other_key.session = "other-session".into();
        assert!(!base.can_replay_snapshot(&snapshot, &other_key, Revision(1), false));
        for mode in 0..7 {
            let mut changed = snapshot.clone();
            match mode {
                0 => changed.app = "other.app".into(),
                1 => changed.display_id = Some(2),
                2 => changed.image.bytes.push(1),
                3 => changed.bracket_equal = false,
                4 => {
                    changed.evidence.window.started_unix_ms -= 5001;
                    changed.evidence.window.finished_unix_ms -= 5001;
                }
                5 => changed.scale *= 2.,
                _ => {
                    changed.xml = format!(
                        "<AppiumAUT>{}</AppiumAUT>",
                        "<XCUIElementTypeOther/>".repeat(351)
                    );
                    changed.cost = super::super::super::outline::xml_cost(&changed.xml).unwrap();
                }
            }
            assert!(!base.can_replay_snapshot(&changed, &base.key, Revision(1), false));
        }
    }
    #[test]
    fn bounds_use_intersections_and_include_raster_uncertainty() {
        assert!(intersects([0., 0., 10., 10.], [8., 8., 10., 10.]));
        let a = pixels();
        let mut b = pixels();
        b.rgba[(10 * 20 + 10) * 4] = 0;
        assert_eq!(changes(&a, &b, mapping()).unwrap(), Some([9., 9., 3., 3.]));
        let p = predicate(&[[1., 2., 3., 4.]], None).unwrap();
        assert!(p.contains("wdRect.x + wdRect.width > 1"));
        assert!(p.contains("XCUIElementTypeSecureTextField"));
        assert!(p.contains("XCUIElementTypeKeyboard"));
    }
    #[test]
    fn drift_outside_queried_regions_refuses() {
        let a = pixels();
        let mut b = pixels();
        b.rgba[19 * 20 * 4] = 0;
        assert!(refreshed(&baseline(), &json!([app()]), &a, &b, &[[0., 0., 5., 5.]]).is_err());
    }
    #[test]
    fn caret_drift_is_admitted_only_inside_an_enumerated_editable_field() {
        let a = pixels();
        let mut b = pixels();
        b.rgba[19 * 20 * 4] = 0;
        let areas = [[10., 10., 5., 5.]];
        let field = row("body", "XCUIElementTypeTextView", [0., 17., 4., 3.]);
        assert!(refreshed(&baseline(), &json!([app(), field]), &a, &b, &areas).is_ok());
        let button = row("body", "XCUIElementTypeButton", [0., 17., 4., 3.]);
        assert!(refreshed(&baseline(), &json!([app(), button]), &a, &b, &areas).is_err());
    }
    #[test]
    fn hidden_global_blockers_and_duplicate_ids_refuse() {
        for kind in [
            "XCUIElementTypeAlert",
            "XCUIElementTypeSheet",
            "XCUIElementTypeSecureTextField",
        ] {
            let mut bad = row("bad", kind, [18., 18., 1., 1.]);
            bad["displayed"] = json!(false);
            assert!(
                refreshed(&baseline(), &json!([app(), bad]), &pixels(), &pixels(), &[]).is_err()
            );
        }
        assert!(
            refreshed(
                &baseline(),
                &json!([app(), app()]),
                &pixels(),
                &pixels(),
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn changed_region_removes_old_identity_and_mints_native_only_handle() {
        let b = baseline();
        let old = b.entries[0].0.public.target_id.clone();
        let out = refreshed(
            &b,
            &json!([app(), row("new", "XCUIElementTypeButton", [3., 3., 3., 3.])]),
            &pixels(),
            &pixels(),
            &[[1., 1., 10., 10.]],
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_ne!(out[0].0.public.target_id, old);
        assert_eq!(out[0].0.native, "new");
        assert_eq!(out[0].0.public.source, "native");
        assert!(!out[0].0.public.can_type);
        let xml = presentation(mapping(), "example.app", &[out[0].0.clone()]);
        assert!(xml.starts_with("<AppiumAUT source=\"native_regions\">"));
        assert!(crate::wda_ui_admission::safe_context(&xml));
    }
    #[test]
    fn untouched_rows_keep_their_original_expiry_not_semantic_freshness() {
        let mut b = baseline();
        let before = b.entries[0].1;
        let out = refreshed(&b, &json!([app()]), &pixels(), &pixels(), &[]).unwrap();
        assert_eq!(out[0].1, before);
        assert_eq!(out[0].0.public.source, "native_cached");
        b.entries[0].1 = Instant::now() - Duration::from_secs(1);
        assert!(
            refreshed(&b, &json!([app()]), &pixels(), &pixels(), &[])
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn consuming_a_target_prevents_its_handle_from_reappearing() {
        let mut b = baseline();
        let target = b.entries[0].0.public.clone();
        b.consume(&target);
        let out = refreshed(
            &b,
            &json!([app(), row("old", "XCUIElementTypeButton", [2., 2., 3., 3.])]),
            &pixels(),
            &pixels(),
            &b.dirty,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_ne!(out[0].0.public.target_id, target.target_id);
    }
    #[test]
    fn malformed_attributes_and_mapping_mismatch_refuse() {
        let mut bad = row("bad", "XCUIElementTypeButton", [1., 1., 1., 1.]);
        bad.as_object_mut().unwrap().remove("attribute/selected");
        assert!(refreshed(&baseline(), &json!([app(), bad]), &pixels(), &pixels(), &[]).is_err());
        let mut b = pixels();
        b.width = 21;
        assert!(changes(&pixels(), &b, mapping()).is_err());
    }
    struct NativeFixture {
        rows: Value,
        calls: std::cell::RefCell<Vec<String>>,
    }
    impl WdaRequest for NativeFixture {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            self.calls.borrow_mut().push(format!("{method} {path}"));
            let value = if path.ends_with("/appium/settings") {
                json!({})
            } else if path.ends_with("/wda/activeAppInfo") {
                json!({"bundleId":"example.app"})
            } else if path.ends_with("/wda/locked") {
                json!(false)
            } else if path.ends_with("/elements") {
                self.rows.clone()
            } else if path.ends_with("/attribute/hittable") {
                json!(true)
            } else if path == "/screenshot" {
                let mut bytes = Vec::new();
                {
                    let mut enc = png::Encoder::new(&mut bytes, 20, 20);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    enc.write_header()
                        .unwrap()
                        .write_image_data(&pixels().rgba)
                        .unwrap();
                }
                json!(base64::engine::general_purpose::STANDARD.encode(bytes))
            } else {
                panic!("unexpected request {path}")
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":value})).unwrap(),
            })
        }
    }
    #[tokio::test]
    async fn incremental_capture_has_no_full_source_request() {
        let mut b = baseline();
        let fixture = NativeFixture {
            rows: json!([app(), row("old", "XCUIElementTypeButton", [2., 2., 3., 3.])]),
            calls: Default::default(),
        };
        let encoded = fixture
            .request("GET", "/screenshot", b"", Duration::from_secs(1))
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&encoded.body).unwrap();
        b.image.bytes = base64::engine::general_purpose::STANDARD
            .decode(result["value"].as_str().unwrap())
            .unwrap();
        let DeltaCapture {
            raw,
            entries,
            effect,
            ..
        } = capture(&fixture, &b, None).await.unwrap();
        assert!(effect.is_none());
        assert!(
            raw.accessibility_tree
                .starts_with("<AppiumAUT source=\"native_regions\">")
        );
        assert_eq!(entries.len(), 1);
        assert!(
            fixture
                .calls
                .borrow()
                .iter()
                .all(|p| !p.contains("/source"))
        );
    }
    #[tokio::test]
    async fn invisible_semantic_changes_still_refuse_at_input_admission() {
        for changed in ["label", "enabled"] {
            let b = baseline();
            let mut current = row("old", "XCUIElementTypeButton", [2., 2., 3., 3.]);
            if changed == "label" {
                current["label"] = json!("different");
            } else {
                current["enabled"] = json!(false);
            }
            let fixture = NativeFixture {
                rows: json!([app(), current]),
                calls: Default::default(),
            };
            let input = Input {
                token: 1,
                text_basis: None,
                key: b.key.clone(),
                app: b.app.clone(),
                bound: b.entries[0].0.clone(),
                action: ElementAction::Tap {
                    target_id: b.entries[0].0.public.target_id.clone(),
                },
                until: Instant::now() + AGE,
            };
            assert!(fast::revalidate(&fixture, &input).await.is_err());
            assert!(fixture.calls.borrow().iter().all(|p| !p.contains("/click")));
        }
    }
    #[test]
    fn retained_semantics_keep_the_original_input_deadline() {
        let mut b = baseline();
        let deadline = Instant::now() + Duration::from_millis(2);
        b.entries[0].1 = deadline;
        b.until = Instant::now() + AGE;
        let targets = Targets {
            key: b.key.clone(),
            revision: Some(Revision(2)),
            app: b.app.clone(),
            until: b.input_deadline(),
            entries: b.entries.iter().map(|(e, _)| e.clone()).collect(),
        };
        assert_eq!(targets.until, deadline);
        assert!(targets.context_matches(
            Some(&b.key),
            Revision(2),
            Some(&b.app),
            deadline - Duration::from_millis(1)
        ));
        assert!(!targets.context_matches(Some(&b.key), Revision(2), Some(&b.app), deadline));
    }
    #[test]
    fn dispatch_failure_cannot_restore_the_consumed_delta_handle() {
        let mut b = baseline();
        let bound = b.entries[0].0.clone();
        let action = ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        };
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        adapter.target_input = Some(Input {
            token: 1,
            text_basis: None,
            key: b.key.clone(),
            app: b.app.clone(),
            bound: bound.clone(),
            action: action.clone(),
            until: Instant::now() + AGE,
        });
        adapter.target_delta = Some(b.clone());
        // Any failure after the one-use boundary, including a lost ack, must
        // leave the same consumed state. This fixture fails before I/O.
        assert!(
            adapter
                .send_native_target(&action, 1, Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        b = adapter.target_delta.take().unwrap();
        assert!(adapter.target_input.is_none());
        assert!(b.entries.is_empty());
        let next = refreshed(
            &b,
            &json!([app(), row("old", "XCUIElementTypeButton", [2., 2., 3., 3.])]),
            &pixels(),
            &pixels(),
            &b.dirty,
        )
        .unwrap();
        assert_ne!(next[0].0.public.target_id, bound.public.target_id);
    }
}
