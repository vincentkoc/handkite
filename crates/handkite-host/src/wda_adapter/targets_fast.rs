//! Target-scoped tap evidence. Full trees are produced by explicit observes.
use super::super::targets_pixels::Pixels;
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Mapping {
    pub viewport: [f64; 4],
    pub scale: f64,
    pub display_id: Option<u64>,
}
const MARKERS: &[&str] = &[
    "proud-lock",
    "passcode-view",
    "call-to-action-label",
    "Enter Passcode",
];

fn predicate(bound: &Bound) -> Result<String, AdapterError> {
    // Keep every duplicate descriptor. A missing old ID is checked directly
    // before absence can prove an effect, even if its label changed.
    let target = input_predicate(
        bound,
        &ElementAction::Tap {
            target_id: bound.public.target_id.clone(),
        },
    )?;
    Ok(format!("({target}) OR ({})", context_predicate()?))
}

pub(super) fn context_predicate() -> Result<String, AdapterError> {
    let mut terms = vec![
        "type IN {'XCUIElementTypeApplication','XCUIElementTypeAlert','XCUIElementTypeSheet','XCUIElementTypeSecureTextField'}".into()];
    // Match the same markers checked after control normalization, without
    // serializing unrelated rows containing common letters.
    let patterns = MARKERS
        .iter()
        .map(|marker| {
            format!(
                "(?s).*{}.*",
                marker
                    .chars()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(r"\p{Cc}*")
            )
        })
        .chain(std::iter::once(r"(?s).*[\p{Cc}&&[^\t\r\n]].*".to_owned()));
    let patterns = patterns
        .map(|p| serde_json::to_string(&p).map_err(|_| AdapterError))
        .collect::<Result<Vec<_>, _>>()?;
    for field in ["name", "label", "value", "placeholderValue"] {
        for pattern in &patterns {
            terms.push(format!("{field} MATCHES[c] {pattern}"));
        }
    }
    Ok(terms
        .into_iter()
        .map(|s| format!("({s})"))
        .collect::<Vec<_>>()
        .join(" OR "))
}

pub(super) fn attrs(row: &Value) -> Result<Attributes, AdapterError> {
    let mut out = Attributes::new();
    out.insert(
        "type".into(),
        row["type"].as_str().ok_or(AdapterError)?.into(),
    );
    for (name, key) in [
        ("label", "label"),
        ("name", "attribute/name"),
        ("value", "attribute/value"),
        ("placeholderValue", "attribute/placeholderValue"),
    ] {
        match row.get(key) {
            Some(Value::String(text)) if text.len() <= 2 * 1024 * 1024 => {
                let normalized: String = text.chars().filter(|c| !c.is_control()).collect();
                if text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
                    || MARKERS.iter().any(|m| normalized.contains(m))
                {
                    return Err(AdapterError);
                }
                out.insert(name.into(), text.clone());
            }
            Some(Value::Null) => {}
            _ => return Err(AdapterError),
        }
    }
    for (name, key) in [
        ("enabled", "enabled"),
        ("visible", "displayed"),
        ("focused", "attribute/focused"),
        ("selected", "attribute/selected"),
    ] {
        let value = row
            .get(key)
            .and_then(|v| {
                v.as_bool().or_else(|| match v.as_str() {
                    Some("true") => Some(true),
                    Some("false") => Some(false),
                    _ => None,
                })
            })
            .ok_or(AdapterError)?;
        out.insert(name.into(), value.to_string());
    }
    for key in ["x", "y", "width", "height"] {
        let value = row["rect"][key]
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or(AdapterError)?;
        out.insert(key.into(), value.to_string());
    }
    Ok(out)
}

pub(super) fn checked_rows(items: &Value, mapping: Mapping) -> Result<Vec<Value>, AdapterError> {
    let rows = items
        .as_array()
        .filter(|r| r.len() <= 2048)
        .ok_or(AdapterError)?;
    let mut ids = BTreeSet::new();
    let mut applications = 0;
    for row in rows {
        if !ids.insert(id(row)?) {
            return Err(AdapterError);
        }
        let a = attrs(row)?;
        match a["type"].as_str() {
            "XCUIElementTypeAlert" | "XCUIElementTypeSheet" | "XCUIElementTypeSecureTextField" => {
                return Err(AdapterError);
            }
            "XCUIElementTypeApplication" => {
                applications += 1;
                let r = &row["rect"];
                if [
                    r["x"].as_f64(),
                    r["y"].as_f64(),
                    r["width"].as_f64(),
                    r["height"].as_f64(),
                ] != mapping.viewport.map(Some)
                {
                    return Err(AdapterError);
                }
            }
            _ => {}
        }
    }
    if applications != 1 {
        return Err(AdapterError);
    }
    Ok(rows.clone())
}

struct Sample {
    rows: Vec<Value>,
    image: ScreenImage,
    pixels: Pixels,
    focused: Option<String>,
    window: ObservationWindow,
    started: Instant,
}

async fn sample(wda: &impl WdaRequest, input: &Input, focus: bool) -> Result<Sample, AdapterError> {
    let _span = crate::timing::Span::new("target_scoped_capture");
    let mapping = input.bound.mapping.ok_or(AdapterError)?;
    if MARKERS.iter().any(|m| input.app.contains(m)) || input.app.chars().any(char::is_control) {
        return Err(AdapterError);
    }
    let started = Instant::now();
    let start_ms = unix_ms()?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    let filter = predicate(&input.bound)?;
    let (image, items, app, focused) = tokio::join!(
        screenshot(wda),
        query_predicate(wda, &input.key.session, &filter),
        app_matches(wda, &input.key.session, &input.app),
        async {
            if focus {
                active(wda, &input.key.session).await.map(Some)
            } else {
                Ok(None)
            }
        },
    );
    app?;
    let image = image?;
    let rows = checked_rows(&items?, mapping)?;
    let pixels = Pixels::decode(&image.bytes)?;
    pixels.validate_mapping(
        mapping.viewport,
        mapping.scale,
        [mapping.viewport[2], mapping.viewport[3]],
    )?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    Ok(Sample {
        rows,
        image,
        pixels,
        focused: focused?,
        window: ObservationWindow {
            started_unix_ms: start_ms,
            finished_unix_ms: unix_ms()?,
        },
        started,
    })
}

#[cfg(test)]
#[path = "targets_fast_tests.rs"]
pub(super) mod tests;

fn matches_count(bound: &Bound, rows: &[Value]) -> usize {
    rows.iter()
        .filter(|row| descriptor_matches(row, &bound.attrs))
        .count()
}

/// The frame most recently presented with the target, at most this old.
pub(super) const FAST_FRAME_AGE: Duration = Duration::from_secs(2);

pub(super) struct FastBasis {
    pub image: ScreenImage,
    pub captured: Instant,
    /// Editable fields whose caret may blink without new semantic capture.
    pub fields: Vec<[f64; 4]>,
}

/// Frame-scoped admission from one image-only grab. `Ok(false)` means the
/// screen drifted outside an editable field, so the full scoped query decides.
/// A changed target region refuses exactly as the scoped path does.
async fn validate_frame(
    wda: &impl WdaRequest,
    input: &Input,
    basis: &FastBasis,
) -> Result<bool, AdapterError> {
    let _span = crate::timing::Span::new("target_frame_admission");
    let mapping = input.bound.mapping.ok_or(AdapterError)?;
    if basis.captured.elapsed() >= FAST_FRAME_AGE {
        return Ok(false);
    }
    if MARKERS.iter().any(|m| input.app.contains(m)) || input.app.chars().any(char::is_control) {
        return Err(AdapterError);
    }
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    let (image, app) = tokio::join!(
        screenshot(wda),
        app_matches(wda, &input.key.session, &input.app)
    );
    app?;
    let pixels = Pixels::decode(&image?.bytes)?;
    pixels.validate_mapping(
        mapping.viewport,
        mapping.scale,
        [mapping.viewport[2], mapping.viewport[3]],
    )?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    let region = pixels.region(mapping.viewport, &input.bound.attrs, &mut 0)?;
    if input.bound.region.as_ref() != Some(&region) {
        return Err(AdapterError);
    }
    let base = Pixels::decode(&basis.image.bytes)?;
    Ok(match super::delta::changes(&base, &pixels, mapping)? {
        None => true,
        Some(changed) => {
            !super::delta::intersects(changed, input.bound.public.rect)
                && basis
                    .fields
                    .iter()
                    .any(|field| super::delta::contains(*field, changed))
        }
    })
}

async fn validate_region(
    wda: &impl WdaRequest,
    input: &Input,
    basis: Option<&FastBasis>,
) -> Result<(), AdapterError> {
    if let Some(basis) = basis
        && validate_frame(wda, input, basis).await?
    {
        return Ok(());
    }
    let _span = crate::timing::Span::new("target_scoped_region");
    let sample = sample(wda, input, false).await?;
    let bound = &input.bound;
    let current = sample
        .rows
        .iter()
        .find(|row| id(row).ok().as_deref() == Some(&bound.native))
        .ok_or(AdapterError)?;
    if matches_count(bound, &sample.rows) != 1
        || current != &bound.rich
        || !identity_matches(current, &bound.attrs)
        || current["enabled"] != true
        || current["displayed"] != true
    {
        return Err(AdapterError);
    }
    let mapping = bound.mapping.ok_or(AdapterError)?;
    let region = sample
        .pixels
        .region(mapping.viewport, &bound.attrs, &mut 0)?;
    if bound.region.as_ref() != Some(&region) {
        return Err(AdapterError);
    }
    Ok(())
}

async fn validate_tail(wda: &impl WdaRequest, input: &Input) -> Result<(), AdapterError> {
    let path = format!(
        "/session/{}/element/{}/attribute/hittable",
        input.key.session, input.bound.native
    );
    if value(wda, &input.key.session, "GET", &path, b"").await? != true {
        return Err(AdapterError);
    }
    app_matches(wda, &input.key.session, &input.app).await
}

#[cfg(test)]
pub(super) async fn revalidate(wda: &impl WdaRequest, input: &Input) -> Result<(), AdapterError> {
    validate_region(wda, input, None).await?;
    validate_tail(wda, input).await
}

pub(super) struct NativeProof {
    pub started: Instant,
    pub deadline: Instant,
}

impl NativeProof {
    pub(super) fn write_deadline(&self) -> Result<Instant, AdapterError> {
        (Instant::now() < self.deadline)
            .then_some(self.deadline)
            .ok_or(AdapterError)
    }
}

pub(super) async fn revalidate_with_native(
    wda: &impl WdaRequest,
    input: &Input,
    deadline: Instant,
    basis: Option<FastBasis>,
    native: impl FnOnce(Instant) -> LockState + Send + 'static,
) -> Result<Result<NativeProof, AdapterError>, AdapterError> {
    let _span = crate::timing::Span::new("target_scoped_admission");
    if Instant::now() >= deadline {
        return Ok(Err(AdapterError));
    }
    let checked = rich_context(wda, &input.key.session, ATTRS, async {
        before_input_deadline(deadline, validate_region(wda, input, basis.as_ref())).await?;
        before_input_deadline(deadline, validate_tail(wda, input)).await
    })
    .await;
    crate::timing::outcome("target_settings_restored", checked.is_ok());
    let checked = checked?;
    if let Err(error) = checked {
        return Ok(Err(error));
    }
    let started = Instant::now();
    // Settings traffic must finish before the freshness clock starts. Only the
    // final app check overlaps the native query; the socket write keeps its cap.
    let proof_deadline = deadline.min(started + Duration::from_millis(500));
    if started >= proof_deadline {
        return Ok(Err(AdapterError));
    }
    let (lock, tail) = capture_with_native(
        move || native(proof_deadline),
        before_input_deadline(
            proof_deadline,
            app_matches(wda, &input.key.session, &input.app),
        ),
    )
    .await;
    if let Err(error) = tail {
        return Ok(Err(error));
    }
    if lock != LockState::Unlocked || Instant::now() >= proof_deadline {
        return Ok(Err(AdapterError));
    }
    Ok(Ok(NativeProof {
        started,
        deadline: proof_deadline,
    }))
}

pub(super) async fn absent(wda: &impl WdaRequest, input: &Input) -> Result<bool, AdapterError> {
    let path = format!(
        "/session/{}/element/{}/attribute/type",
        input.key.session, input.bound.native
    );
    let response = wda
        .request("GET", &path, b"", Duration::from_secs(5))
        .await
        .map_err(|_| AdapterError)?;
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    if value["sessionId"].as_str() != Some(&input.key.session) {
        return Err(AdapterError);
    }
    Ok(response.status == 404
        && matches!(
            value["value"]["error"].as_str(),
            Some("stale element reference" | "no such element")
        ))
}

fn boolean(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| match value.as_str() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    })
}

pub(super) fn focus_target(bound: &Bound) -> bool {
    matches!(
        bound.public.kind.as_str(),
        "XCUIElementTypeTextField" | "XCUIElementTypeTextView" | "XCUIElementTypeSearchField"
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Same identity with a changed state, value or focus.
    Changed,
    Unconfirmed,
    /// No row or descriptor remains and the region changed. Changed still
    /// requires the exact cached-ID stale response.
    Absent,
}

/// One post-input sample: rows include the acted descriptor wherever it is.
pub(super) fn classify(
    bound: &Bound,
    rows: &[Value],
    region: &super::super::targets_pixels::Region,
    focused: Option<&str>,
) -> Outcome {
    let current = rows
        .iter()
        .find(|row| id(row).ok().as_deref() == Some(&bound.native));
    let same_identity = |row: &Value| {
        ["type", "label", "attribute/name"]
            .iter()
            .all(|key| row.get(*key) == bound.rich.get(*key))
    };
    if let Some(current) = current {
        let changed = same_identity(current)
            && ((bound.focused == Some(false)
                && focused == Some(bound.native.as_str())
                && focus_target(bound))
                || current.get("rect") == bound.rich.get("rect")
                    && (["enabled", "displayed", "attribute/selected"]
                        .iter()
                        .any(|key| boolean(&current[*key]) != boolean(&bound.rich[*key]))
                        || current.get("attribute/value") != bound.rich.get("attribute/value")));
        if changed {
            Outcome::Changed
        } else {
            Outcome::Unconfirmed
        }
    } else if matches_count(bound, rows) == 0
        && bound.region.as_ref().is_some_and(|before| before != region)
    {
        Outcome::Absent
    } else {
        Outcome::Unconfirmed
    }
}

pub(super) async fn after(
    wda: &impl WdaRequest,
    input: &Input,
) -> Result<(RawObservation, Vec<Bound>, bool, Instant), AdapterError> {
    let focus = focus_target(&input.bound);
    let sample = sample(wda, input, focus).await?;
    let bound = &input.bound;
    let mapping = bound.mapping.ok_or(AdapterError)?;
    let region = sample
        .pixels
        .region(mapping.viewport, &bound.attrs, &mut 0)?;
    let current = sample
        .rows
        .iter()
        .find(|row| id(row).ok().as_deref() == Some(&bound.native));
    let verified = match classify(bound, &sample.rows, &region, sample.focused.as_deref()) {
        Outcome::Changed => true,
        Outcome::Unconfirmed => false,
        Outcome::Absent => absent(wda, input).await?,
    };
    app_matches(wda, &input.key.session, &input.app).await?;
    let window = ObservationWindow {
        started_unix_ms: sample.window.started_unix_ms,
        finished_unix_ms: unix_ms()?,
    };
    let mut next = Vec::new();
    // An unchanged native row can retain the original XML-backed binding. A changed
    // row is reported by the effect, never promoted into a new text capability.
    if let Some(current) =
        current.filter(|row| *row == &bound.rich && matches_count(bound, &sample.rows) == 1)
    {
        let mut updated = bound.clone();
        let mut random = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
            .map_err(|_| AdapterError)?;
        updated.public.target_id = format!(
            "t_{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        updated.public.can_type = false;
        updated.public.text_only = false;
        updated.rich = current.clone();
        updated.region = Some(region);
        updated.focused = sample.focused.as_ref().map(|id| id == &bound.native);
        next.push(updated);
    }
    Ok((
        RawObservation {
            connected_usb: input.key.device.clone(),
            wda_target: input.key.device.clone(),
            window,
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some(input.app.clone()),
            },
            accessibility_tree: String::new(),
            screenshot: sample.image,
        },
        next,
        verified,
        sample.started,
    ))
}
