//! Native text admission from a recent full context and fresh native facts.
use super::*;

pub(super) fn basis(snapshot: &Snapshot, bound: &Bound) -> Option<Snapshot> {
    if bound.ocr.is_some() || !bound.public.can_type || bound.focused != Some(true) {
        return None;
    }
    if !snapshot.evidence.reusable().ok()? {
        return None;
    }
    let xml = native_visibility(&snapshot.xml, &snapshot.items).ok()?;
    let context = crate::wda_ui_admission::selected_text_snapshot(&xml, &bound.attrs)?;
    if !ordinary(&context) || !same_target(bound, &candidates(&xml, &snapshot.items).ok()?) {
        return None;
    }
    Some(snapshot.clone())
}

fn ordinary(context: &crate::wda_ui_admission::TextSnapshot) -> bool {
    context.emoji_search.is_none()
        && context
            .context
            .iter()
            .filter(|field| {
                field.get("type").map(String::as_str) != Some("XCUIElementTypeKeyboard")
                    && field.get("visible").map(String::as_str) == Some("true")
                    && field.get("enabled").map(String::as_str) == Some("true")
            })
            .count()
            == 1
}

pub(super) async fn revalidate(
    wda: &impl WdaRequest,
    input: &Input,
    snapshot: &Snapshot,
) -> Result<(), AdapterError> {
    let _timing = crate::timing::Span::new("target_text_admission");
    let bound = &input.bound;
    let mapping = bound.mapping.ok_or(AdapterError)?;
    if snapshot.key != input.key || snapshot.app != input.app || !snapshot.evidence.reusable()? {
        return Err(AdapterError);
    }
    let xml = native_visibility(&snapshot.xml, &snapshot.items)?;
    let context = crate::wda_ui_admission::selected_text_snapshot(&xml, &bound.attrs)
        .filter(ordinary)
        .ok_or(AdapterError)?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    app_matches(wda, &input.key.session, &input.app).await?;
    let before = screenshot(wda).await?;
    let filter = format!(
        "({}) OR ({})",
        input_predicate(bound, &input.action)?,
        fast::context_predicate()?,
    );
    let window_path = format!("/session/{}/window/size", input.key.session);
    let (items, focused, window, scale) = tokio::join!(
        query_predicate(wda, &input.key.session, &filter),
        active(wda, &input.key.session),
        value(wda, &input.key.session, "GET", &window_path, b""),
        super::super::display::screen_scale(wda, mapping.display_id),
    );
    let rows = fast::checked_rows(&items?, mapping)?;
    if focused? != bound.native || scale? != mapping.scale {
        return Err(AdapterError);
    }
    let window = window?;
    let dimensions = [
        window["width"].as_f64().ok_or(AdapterError)?,
        window["height"].as_f64().ok_or(AdapterError)?,
    ];
    let fields = Value::Array(
        rows.into_iter()
            .filter(|row| editable_or_keyboard(row["type"].as_str()))
            .collect(),
    );
    let recipient = super::super::focused_text::field_from_context(&fields, &context)?;
    if recipient.0 != bound.native
        || !complete_text_context(&xml, &fields)
        || !same_target(bound, &candidates(&xml, &fields)?)
    {
        return Err(AdapterError);
    }
    let after = screenshot(wda).await?;
    for image in [&before, &after] {
        let pixels = super::super::targets_pixels::Pixels::decode(&image.bytes)?;
        pixels.validate_mapping(mapping.viewport, mapping.scale, dimensions)?;
        let region = pixels.region(mapping.viewport, &bound.attrs, &mut 0)?;
        if !bound.public.text_only && bound.region.as_ref() != Some(&region) {
            return Err(AdapterError);
        }
    }
    let _focus = crate::timing::Span::new("target_text_final_focus");
    let path = format!(
        "/session/{}/element/{}/attribute/hittable",
        input.key.session, bound.native
    );
    let (hittable, focused, app) = tokio::join!(
        value(wda, &input.key.session, "GET", &path, b""),
        active(wda, &input.key.session),
        app_matches(wda, &input.key.session, &input.app),
    );
    app?;
    if hittable? != true || focused? != bound.native {
        return Err(AdapterError);
    }
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    if !snapshot.evidence.reusable()? {
        return Err(AdapterError);
    }
    Ok(())
}
