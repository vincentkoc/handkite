use super::*;
use crate::wda_ui_admission::Attributes;
use serde_json::{Value, json};

pub(super) const ATTRIBUTES: &str = "type,enabled,displayed,rect,label,attribute/name,attribute/value,attribute/placeholderValue,attribute/focused";
const MARKERS: [&str; 4] = [
    "proud-lock",
    "passcode-view",
    "call-to-action-label",
    "Enter Passcode",
];

pub(super) struct Input {
    pub(super) token: u64,
    pub(super) key: cache::Key,
    pub(super) field: String,
    pub(super) before: String,
    pub(super) text: String,
    pub(super) app: String,
    pub(super) until: Instant,
    pub(super) sent: bool,
}

pub(super) async fn value(
    wda: &impl WdaRequest,
    session: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Value, AdapterError> {
    let response = wda
        .request(method, path, body, Duration::from_secs(5))
        .await
        .map_err(|_| AdapterError)?;
    if response.status != 200 {
        return Err(AdapterError);
    }
    let mut response: Value = serde_json::from_slice(&response.body).map_err(|_| AdapterError)?;
    if response["sessionId"].as_str() != Some(session) || response["value"].get("error").is_some() {
        return Err(AdapterError);
    }
    response
        .get_mut("value")
        .map(Value::take)
        .ok_or(AdapterError)
}

pub(super) fn id(item: &Value) -> Result<String, AdapterError> {
    let read = |key| match item.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|id| crate::wda_action::valid_session_id(id))
            .map(Some)
            .ok_or(AdapterError),
    };
    let a = read("ELEMENT")?;
    let b = read("element-6066-11e4-a52e-4f735466cecf")?;
    if a.zip(b).is_some_and(|(a, b)| a != b) {
        return Err(AdapterError);
    }
    a.or(b).map(str::to_owned).ok_or(AdapterError)
}

fn settings_pair(value: &Value) -> Result<Value, AdapterError> {
    let compact = value["shouldUseCompactResponses"]
        .as_bool()
        .ok_or(AdapterError)?;
    let attrs = value["elementResponseAttributes"]
        .as_str()
        .ok_or(AdapterError)?;
    Ok(json!({"shouldUseCompactResponses": compact, "elementResponseAttributes": attrs}))
}

/// Bracket the process-wide response format. No input runs before exact restoration.
async fn context(
    wda: &impl WdaRequest,
    session: &str,
) -> Result<Result<(Value, Instant, u64), AdapterError>, AdapterError> {
    let _format = crate::timing::Span::new("text_context_transaction");
    rich_context(wda, session, ATTRIBUTES, async {
        let types = [
            "Alert",
            "Sheet",
            "SecureTextField",
            "TextField",
            "TextView",
            "SearchField",
            "Keyboard",
        ];
        let mut terms: Vec<String> = types
            .iter()
            .map(|kind| format!("type == 'XCUIElementType{kind}'"))
            .collect();
        for attr in ["name", "label", "value", "placeholderValue"] {
            for marker in MARKERS {
                terms.push(format!("{attr} CONTAINS '{marker}'"));
            }
        }
        let predicate = terms.join(" OR ");
        let items = value(
            wda,
            session,
            "POST",
            &format!("/session/{session}/elements"),
            &serde_json::to_vec(&json!({"using":"predicate string", "value":predicate}))
                .map_err(|_| AdapterError)?,
        )
        .await?;
        Ok((items, Instant::now(), unix_ms()?))
    })
    .await
}

/// Restoration is outside the cancellable work window. Callers must await this
/// whole transaction; an unknown restoration poisons all subsequent input.
pub(super) async fn rich_context<T>(
    wda: &impl WdaRequest,
    session: &str,
    attributes: &str,
    work: impl Future<Output = Result<T, AdapterError>>,
) -> Result<Result<T, AdapterError>, AdapterError> {
    let path = format!("/session/{session}/appium/settings");
    let original = settings_pair(&value(wda, session, "GET", &path, b"").await?)?;
    if settings_pair(&value(wda, session, "GET", &path, b"").await?)? != original {
        return Err(AdapterError);
    }
    let target = json!({"shouldUseCompactResponses":false,"elementResponseAttributes":attributes});
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        value(
            wda,
            session,
            "POST",
            &path,
            &serde_json::to_vec(&json!({"settings":target})).map_err(|_| AdapterError)?,
        )
        .await?;
        if settings_pair(&value(wda, session, "GET", &path, b"").await?)? != target {
            return Err(AdapterError);
        }
        work.await
    })
    .await
    .unwrap_or(Err(AdapterError));
    let restored = tokio::time::timeout(Duration::from_secs(15), async {
        let current = settings_pair(&value(wda, session, "GET", &path, b"").await?)?;
        if current != original {
            if current != target {
                return Err(AdapterError);
            }
            value(
                wda,
                session,
                "POST",
                &path,
                &serde_json::to_vec(&json!({"settings":original})).map_err(|_| AdapterError)?,
            )
            .await?;
        }
        if settings_pair(&value(wda, session, "GET", &path, b"").await?)? != original {
            return Err(AdapterError);
        }
        Ok(())
    })
    .await
    .map_err(|_| AdapterError)?;
    restored?;
    Ok(result)
}

fn rect(attrs: &Attributes) -> Option<[f64; 4]> {
    let number = |key| {
        attrs
            .get(key)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };
    Some([
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    ])
}

fn field_from_context(
    items: &Value,
    snapshot: &crate::wda_ui_admission::TextSnapshot,
) -> Result<(String, String), AdapterError> {
    let baseline = &snapshot.field;
    let screen = rect(&snapshot.screen).ok_or(AdapterError)?;
    let items = items
        .as_array()
        .filter(|items| {
            !items.is_empty() && items.len() <= 64 && items.len() == snapshot.context.len()
        })
        .ok_or(AdapterError)?;
    let mut ids = std::collections::BTreeSet::new();
    let mut matched = std::collections::BTreeSet::new();
    let mut field = None;
    let mut keyboards = 0;
    let mut emoji_search_seen = false;
    for item in items {
        let identity = id(item)?;
        if !ids.insert(identity.clone()) {
            return Err(AdapterError);
        }
        let kind = item["type"].as_str().ok_or(AdapterError)?;
        let kind = kind.strip_prefix("XCUIElementType").unwrap_or(kind);
        if matches!(kind, "SecureTextField" | "Alert" | "Sheet") {
            return Err(AdapterError);
        }
        if !matches!(kind, "TextField" | "TextView" | "SearchField" | "Keyboard") {
            return Err(AdapterError);
        }
        let attrs = context_attributes(item)?;
        let displayed = item["displayed"].as_bool().ok_or(AdapterError)?;
        let enabled = item["enabled"].as_bool().ok_or(AdapterError)?;
        // This attribute is false on iOS even for /element/active. The active
        // endpoint's identity is checked immediately before input instead.
        let focused = item["attribute/focused"].as_bool().ok_or(AdapterError)?;
        let mut matches = snapshot.context.iter().enumerate().filter(|(_, expected)| {
            context_matches(item, &attrs, expected)
                && expected.get("visible").map(String::as_str)
                    == Some(if displayed { "true" } else { "false" })
                && expected.get("enabled").map(String::as_str)
                    == Some(if enabled { "true" } else { "false" })
        });
        let (index, expected) = matches.next().ok_or(AdapterError)?;
        if matches.next().is_some() || !matched.insert(index) {
            return Err(AdapterError);
        }
        if !displayed || !enabled {
            continue;
        }
        if !matches!(kind, "TextField" | "TextView" | "SearchField" | "Keyboard") {
            return Err(AdapterError);
        }
        let mut bounds = [0.0; 4];
        for (index, name) in ["x", "y", "width", "height"].iter().enumerate() {
            bounds[index] = item["rect"][name]
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or(AdapterError)?;
        }
        let [x, y, w, h] = bounds;
        if x < screen[0]
            || y < screen[1]
            || w <= 0.0
            || h <= 0.0
            || x + w > screen[0] + screen[2]
            || y + h > screen[1] + screen[3]
        {
            return Err(AdapterError);
        }
        if kind == "Keyboard" {
            if let Some(search) = &snapshot.emoji_search
                && !context_matches(item, &attrs, &search.keyboard)
            {
                return Err(AdapterError);
            }
            keyboards += 1;
            continue;
        }
        if let Some(search) = &snapshot.emoji_search
            && context_matches(item, &attrs, &search.field)
        {
            if emoji_search_seen || focused {
                return Err(AdapterError);
            }
            emoji_search_seen = true;
            continue;
        }
        if expected != baseline {
            continue;
        }
        if field.is_some() || rect(baseline) != Some(bounds) {
            return Err(AdapterError);
        }
        for key in ["type", "name", "label", "placeholderValue"] {
            if attrs.get(key) != baseline.get(key) {
                return Err(AdapterError);
            }
        }
        let before = match item.get("attribute/value") {
            // WDA represents an empty editor as null; its full XML may omit value.
            // Normalize only this otherwise admitted public editor, never a missing key.
            Some(Value::Null) if baseline.get("value").is_none_or(String::is_empty) => {
                String::new()
            }
            Some(Value::String(value)) if baseline.get("value") == Some(value) => value.clone(),
            _ => return Err(AdapterError),
        };
        field = Some((identity, before));
    }
    if keyboards != 1 || emoji_search_seen != snapshot.emoji_search.is_some() {
        return Err(AdapterError);
    }
    field.ok_or(AdapterError)
}

fn context_attributes(item: &Value) -> Result<Attributes, AdapterError> {
    let kind = item["type"].as_str().ok_or(AdapterError)?;
    let kind = kind.strip_prefix("XCUIElementType").unwrap_or(kind);
    if !matches!(kind, "TextField" | "TextView" | "SearchField" | "Keyboard") {
        return Err(AdapterError);
    }
    let mut attrs = Attributes::new();
    attrs.insert("type".into(), format!("XCUIElementType{kind}"));
    for (source, dest) in [
        ("label", "label"),
        ("attribute/name", "name"),
        ("attribute/value", "value"),
        ("attribute/placeholderValue", "placeholderValue"),
    ] {
        let content = item.get(source).ok_or(AdapterError)?;
        if content.is_null() {
            continue;
        }
        let content = content.as_str().ok_or(AdapterError)?;
        if MARKERS.iter().any(|marker| content.contains(marker)) {
            return Err(AdapterError);
        }
        attrs.insert(dest.into(), content.into());
    }
    Ok(attrs)
}

fn active_field_from_context(
    items: &Value,
    xml: &str,
    active: &str,
) -> Result<(String, String), AdapterError> {
    let (_, fields) = crate::wda_ui_admission::text_context(xml).ok_or(AdapterError)?;
    let rows = items.as_array().ok_or(AdapterError)?;
    let mut native = rows.iter().filter(|row| id(row).as_deref() == Ok(active));
    let item = native.next().ok_or(AdapterError)?;
    if native.next().is_some() {
        return Err(AdapterError);
    }
    let attrs = context_attributes(item)?;
    let mut candidates = fields
        .iter()
        .filter(|field| context_matches(item, &attrs, field));
    let selected = candidates.next().ok_or(AdapterError)?;
    if candidates.next().is_some() {
        return Err(AdapterError);
    }
    let snapshot =
        crate::wda_ui_admission::selected_text_snapshot(xml, selected).ok_or(AdapterError)?;
    let recipient = field_from_context(items, &snapshot)?;
    if recipient.0 != active {
        return Err(AdapterError);
    }
    Ok(recipient)
}

fn context_matches(item: &Value, attrs: &Attributes, expected: &Attributes) -> bool {
    let bounds_match = ["x", "y", "width", "height"].iter().all(|key| {
        item["rect"][*key].as_f64().is_some_and(|actual| {
            expected
                .get(*key)
                .and_then(|value| value.parse::<f64>().ok())
                == Some(actual)
        })
    });
    bounds_match
        && ["type", "name", "label", "placeholderValue"]
            .iter()
            .all(|key| attrs.get(*key) == expected.get(*key))
        && match item.get("attribute/value") {
            Some(Value::Null) => expected.get("value").is_none_or(String::is_empty),
            Some(Value::String(value)) => expected.get("value") == Some(value),
            _ => false,
        }
}

pub(super) async fn active(wda: &impl WdaRequest, session: &str) -> Result<String, AdapterError> {
    id(&value(
        wda,
        session,
        "GET",
        &format!("/session/{session}/element/active"),
        b"",
    )
    .await?)
}

pub(super) async fn app_matches(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
) -> Result<(), AdapterError> {
    let app_path = format!("/session/{session}/wda/activeAppInfo");
    let lock_path = format!("/session/{session}/wda/locked");
    let (foreground, locked) = tokio::join!(
        value(wda, session, "GET", &app_path, b""),
        value(wda, session, "GET", &lock_path, b"")
    );
    if foreground?["bundleId"].as_str() != Some(app) || locked? != false {
        return Err(AdapterError);
    }
    Ok(())
}

async fn context_in_app(
    wda: &impl WdaRequest,
    session: &str,
    app: &str,
) -> Result<Result<(Value, String, Instant, u64), AdapterError>, AdapterError> {
    let (app, query, active) = tokio::join!(
        app_matches(wda, session, app),
        context(wda, session),
        active(wda, session)
    );
    // Restoration errors must still poison the adapter, including on app mismatch.
    let query = query?;
    Ok(app
        .and(query)
        .and_then(|(items, at, unix_ms)| Ok((items, active?, at, unix_ms))))
}

async fn dispatch_to_focused(
    wda: &impl WdaRequest,
    session: &str,
    field: &str,
    app: &str,
    action: &Action,
    motion: Option<handkite_core::protocol::Motion>,
) -> Result<TransportReceipt, AdapterDiagnostic> {
    let focus_timing = crate::timing::Span::new("text_final_focus");
    let (active, app) = tokio::join!(active(wda, session), app_matches(wda, session, app));
    if active.map_err(|_| AdapterDiagnostic::PreActionCapture)? != field || app.is_err() {
        return Err(AdapterDiagnostic::PreActionCapture);
    }
    drop(focus_timing);
    WdaActions::execute_on_diagnostic(wda, action, session, motion).await
}

impl WdaAdapter {
    pub(super) fn prepare_focus(
        &mut self,
        action: &Action,
        baseline: &handkite_core::protocol::Observation,
    ) -> Result<Option<handkite_core::controller::PreparedText>, AdapterError> {
        self.focused_text = None;
        let Action::TextCurrent { value: text } = action else {
            return Ok(None);
        };
        if text.is_empty() {
            return Ok(None);
        }
        if crate::wda_ui_admission::text_context(&baseline.accessibility_tree).is_none() {
            return Ok(None);
        }
        let Some(key) = self.cache_key() else {
            return Ok(None);
        };
        let app = baseline
            .state
            .foreground_app
            .as_deref()
            .ok_or(AdapterError)?;
        let _timing = crate::timing::Span::new("text_prepare");
        let start = unix_ms()?;
        self.validate_owned()?;
        if self.native_hint() != LockState::Unlocked {
            return Err(AdapterError);
        }
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let query = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(context_in_app(&wda, &key.session, app));
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                self.text_response_poisoned = true;
                return Err(error);
            }
        };
        self.validate_owned()?;
        if self.cache_key().as_ref() != Some(&key) {
            return Err(AdapterError);
        }
        let (items, active, captured_at, finished_unix_ms) = match query {
            Ok(query) => query,
            Err(_) => return Ok(None),
        };
        let (field, before) =
            match active_field_from_context(&items, &baseline.accessibility_tree, &active) {
                Ok(field) => field,
                Err(_) => return Ok(None),
            };
        self.text_token = self.text_token.checked_add(1).ok_or(AdapterError)?;
        let until = captured_at + TEXT_INPUT_AGE;
        self.focused_text = Some(Input {
            token: self.text_token,
            key,
            field,
            before,
            text: text.clone(),
            app: app.into(),
            until,
            sent: false,
        });
        crate::timing::outcome("text_prepared", true);
        Ok(Some(handkite_core::controller::PreparedText {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            window: ObservationWindow {
                started_unix_ms: start,
                finished_unix_ms,
            },
            state: baseline.state.clone(),
            token: self.text_token,
            valid_until: until,
        }))
    }

    pub(super) fn execute_focus(
        &mut self,
        action: &Action,
        token: u64,
        deadline: Instant,
        motion: Option<handkite_core::protocol::Motion>,
    ) -> Result<TransportReceipt, AdapterError> {
        let mut input = self.focused_text.take().ok_or(AdapterError)?;
        if input.sent
            || input.token != token
            || !matches!(action, Action::TextCurrent { value } if *value == input.text)
            || self.cache_key().as_ref() != Some(&input.key)
            || deadline > input.until
            || Instant::now() >= deadline
        {
            return Err(AdapterError);
        }
        self.cache.clear_full();
        self.validate_owned_with_deadline(Some(deadline))?;
        if self.native_fresh(Some(deadline)) != LockState::Unlocked {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?))
            .with_write_deadline(deadline);
        let receipt = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(async {
                self.actions
                    .validate_existing_diagnostic(&wda)
                    .await
                    .map_err(|_| AdapterDiagnostic::SessionCheck)?;
                dispatch_to_focused(
                    &wda,
                    &input.key.session,
                    &input.field,
                    &input.app,
                    action,
                    motion,
                )
                .await
            })
            .map_err(|reason| {
                self.error_diagnostic = Some(reason);
                AdapterError
            })?;
        input.sent = true;
        self.focused_text = Some(input);
        self.validate_owned()
            .inspect_err(|_| self.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck))?;
        Ok(receipt)
    }

    pub(super) fn read_focus(
        &mut self,
        token: u64,
    ) -> Result<handkite_core::controller::TextReadback, AdapterError> {
        let input = self
            .focused_text
            .take()
            .filter(|input| input.sent && input.token == token)
            .ok_or(AdapterError)?;
        if self.cache_key().as_ref() != Some(&input.key) {
            return Err(AdapterError);
        }
        self.validate_owned()?;
        let verified = {
            let _readback = crate::timing::Span::new("text_readback");
            self.runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(async {
                    let wda = UsbWda::new(self.usb.clone());
                    let path = format!(
                        "/session/{}/element/{}/attribute/value",
                        input.key.session, input.field
                    );
                    let (after, app) = tokio::join!(
                        value(&wda, &input.key.session, "GET", &path, b""),
                        app_matches(&wda, &input.key.session, &input.app)
                    );
                    Ok::<_, AdapterError>(
                        app.is_ok() && inserted_value(&input.before, &input.text, &after?),
                    )
                })
                .unwrap_or(false)
        };
        let started = Instant::now();
        let started_unix_ms = unix_ms()?;
        let mut timings = crate::timing::enabled().then(CaptureTimings::default);
        let frame = self.capture_frame(timings.as_mut());
        if let Some(mut timings) = timings {
            timings.finish(started, frame.is_ok());
            crate::timing::capture(timings);
        }
        let frame = frame?;
        let verified = verified && frame.app == input.app && frame.reported == Some(false);
        let image = RawImageObservation {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            window: ObservationWindow {
                started_unix_ms,
                finished_unix_ms: unix_ms()?,
            },
            screenshot: frame.screenshot,
        };
        let verified = verified && self.cache_key().as_ref() == Some(&input.key);
        crate::timing::outcome("text_verified", verified);
        Ok(handkite_core::controller::TextReadback {
            image,
            input_verified: verified,
        })
    }
}

fn inserted_value(before: &str, text: &str, after: &Value) -> bool {
    after
        .as_str()
        .is_some_and(|after| inserted(before, text, after))
}

/// One insertion/replacement at the current selection; no old caret is inferred.
fn inserted(before: &str, text: &str, after: &str) -> bool {
    if text.is_empty() || before == after || after.len() > before.len() + text.len() {
        return false;
    }
    let removed = before.len() + text.len() - after.len();
    if removed > before.len() {
        return false;
    }
    after
        .char_indices()
        .filter(|(at, _)| after[*at..].starts_with(text))
        .any(|(at, _)| {
            before.is_char_boundary(at)
                && before.is_char_boundary(at + removed)
                && before.get(..at) == after.get(..at)
                && before.get(at + removed..) == after.get(at + text.len()..)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const XML: &str = r#"<AppiumAUT><XCUIElementTypeApplication x="0" y="0" width="400" height="800"><XCUIElementTypeTextView value="Alpha&#10;" visible="true" enabled="true" x="0" y="100" width="400" height="300"/><XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="500" width="400" height="300"/></XCUIElementTypeApplication></AppiumAUT>"#;
    const EMOJI_XML: &str = r#"<AppiumAUT>
<XCUIElementTypeApplication x="0" y="0" width="400" height="800">
  <XCUIElementTypeWindow x="0" y="0" width="400" height="800">
    <XCUIElementTypeTextView visible="true" enabled="true" x="0" y="100" width="400" height="300"/>
  </XCUIElementTypeWindow>
  <XCUIElementTypeWindow x="0" y="400" width="400" height="400">
    <XCUIElementTypeOther visible="true" enabled="true" x="0" y="400" width="400" height="400">
      <XCUIElementTypeOther>
        <XCUIElementTypeTextField value="Search Emoji" placeholderValue="Search Emoji" visible="true" enabled="true" x="10" y="450" width="380" height="40"/>
      </XCUIElementTypeOther>
      <XCUIElementTypeOther>
        <XCUIElementTypeKeyboard visible="true" enabled="true" x="0" y="500" width="400" height="300"/>
      </XCUIElementTypeOther>
    </XCUIElementTypeOther>
  </XCUIElementTypeWindow>
</XCUIElementTypeApplication></AppiumAUT>"#;

    #[test]
    fn emoji_keyboard_search_keeps_primary_editor_preparable() {
        assert!(!crate::wda_ui_admission::visible_current_text_input(
            EMOJI_XML
        ));
        assert!(crate::wda_ui_admission::current_text_snapshot(EMOJI_XML).is_some());
    }

    fn emoji_items() -> Value {
        let mut data = items();
        data[0]["attribute/value"] = Value::Null;
        let mut search = element("TextField", "emoji-search");
        search["attribute/value"] = json!("Search Emoji");
        search["attribute/placeholderValue"] = json!("Search Emoji");
        search["rect"] = json!({"x":10,"y":450,"width":380,"height":40});
        data.as_array_mut().unwrap().push(search);
        data
    }

    #[test]
    fn emoji_context_requires_every_captured_field() {
        let snapshot = crate::wda_ui_admission::current_text_snapshot(EMOJI_XML).unwrap();
        assert_eq!(
            field_from_context(&emoji_items(), &snapshot).unwrap(),
            ("editor".into(), String::new())
        );
        for (key, value) in [
            ("attribute/value", json!("changed")),
            ("attribute/placeholderValue", json!("changed")),
            ("attribute/name", json!("changed")),
            ("label", json!("changed")),
            ("attribute/focused", json!(true)),
            ("attribute/focused", Value::Null),
            ("enabled", json!(false)),
            ("displayed", json!(false)),
            ("rect", json!({"x":11,"y":450,"width":380,"height":40})),
        ] {
            let mut changed = emoji_items();
            changed[2][key] = value;
            assert!(field_from_context(&changed, &snapshot).is_err(), "{key}");
        }
        assert!(field_from_context(&items(), &snapshot).is_err());
        let mut duplicate = emoji_items();
        let extra = duplicate[2].clone();
        duplicate.as_array_mut().unwrap().push(extra);
        assert!(field_from_context(&duplicate, &snapshot).is_err());
        let mut extra = emoji_items();
        extra
            .as_array_mut()
            .unwrap()
            .push(element("TextView", "another-editor"));
        assert!(field_from_context(&extra, &snapshot).is_err());
        let mut changed_keyboard = emoji_items();
        changed_keyboard[1]["label"] = json!("changed");
        assert!(field_from_context(&changed_keyboard, &snapshot).is_err());
    }

    #[test]
    fn emoji_baseline_requires_separate_keyboard_container_and_public_signature() {
        let primary = "<XCUIElementTypeTextView visible=\"true\" enabled=\"true\" x=\"0\" y=\"100\" width=\"400\" height=\"300\"/>";
        let container = "<XCUIElementTypeOther visible=\"true\" enabled=\"true\" x=\"0\" y=\"400\" width=\"400\" height=\"400\">";
        let primary_in_container = EMOJI_XML
            .replace(primary, "")
            .replace(container, &format!("{container}{primary}"));
        for bad in [
            EMOJI_XML.replace("Search Emoji", "Search"),
            EMOJI_XML.replace("<XCUIElementTypeTextField ", "<XCUIElementTypeTextField focused=\"true\" "),
            EMOJI_XML.replace("<XCUIElementTypeTextField ", "<XCUIElementTypeTextField focused=\"unknown\" "),
            EMOJI_XML.replace("<XCUIElementTypeTextView ", "<XCUIElementTypeTextView focused=\"false\" "),
            EMOJI_XML.replace("<XCUIElementTypeTextField value=\"Search Emoji\" placeholderValue=\"Search Emoji\" visible=\"true\"", "<XCUIElementTypeTextField value=\"Search Emoji\" placeholderValue=\"Search Emoji\""),
            EMOJI_XML.replace(container, "<XCUIElementTypeOther visible=\"true\" enabled=\"true\" x=\"0\" y=\"400\" width=\"400\" height=\"100\">"),
            EMOJI_XML.replace("</XCUIElementTypeApplication>", &format!("{primary}</XCUIElementTypeApplication>")),
            EMOJI_XML.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeSecureTextField visible=\"false\"/></XCUIElementTypeApplication>"),
            EMOJI_XML.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeAlert visible=\"false\"/></XCUIElementTypeApplication>"),
            primary_in_container,
        ] {
            assert!(crate::wda_ui_admission::current_text_snapshot(&bad).is_none());
        }
        let split = EMOJI_XML.replace(
            "      </XCUIElementTypeOther>\n      <XCUIElementTypeOther>\n        <XCUIElementTypeKeyboard",
            "      </XCUIElementTypeOther></XCUIElementTypeOther><XCUIElementTypeOther><XCUIElementTypeOther>\n        <XCUIElementTypeKeyboard",
        );
        assert_ne!(split, EMOJI_XML);
        assert!(crate::wda_ui_admission::current_text_snapshot(&split).is_none());
    }

    fn element(kind: &str, id: &str) -> Value {
        let editor = kind == "TextView";
        json!({"ELEMENT":id,"element-6066-11e4-a52e-4f735466cecf":id,"type":format!("XCUIElementType{kind}"),
               "enabled":true,"displayed":true,"attribute/focused":false,
               "label":null,"attribute/name":null,"attribute/placeholderValue":null,
               "attribute/value":if editor { Some("Alpha\n") } else { None },
               "rect":{"x":0,"y":if editor { 100 } else { 500 },"width":400,"height":300}})
    }
    fn items() -> Value {
        json!([
            element("TextView", "editor"),
            element("Keyboard", "keyboard")
        ])
    }
    fn classify(items: &Value) -> Result<(String, String), AdapterError> {
        let snapshot = crate::wda_ui_admission::current_text_snapshot(XML).unwrap();
        field_from_context(items, &snapshot)
    }

    fn multiple_fields() -> (String, Value) {
        let mut extra = String::new();
        let mut data = items();
        for index in 0..5 {
            let y = 410 + index * 15;
            extra.push_str(&format!(
                r#"<XCUIElementTypeTextField name="field-{index}" visible="true" enabled="true" focused="false" x="10" y="{y}" width="200" height="12"/>"#
            ));
            let mut field = element("TextField", &format!("field-{index}"));
            field["attribute/name"] = json!(format!("field-{index}"));
            field["rect"] = json!({"x":10,"y":y,"width":200,"height":12});
            data.as_array_mut().unwrap().push(field);
        }
        (
            XML.replace(
                "</XCUIElementTypeApplication>",
                &format!("{extra}</XCUIElementTypeApplication>"),
            ),
            data,
        )
    }

    #[test]
    fn native_active_identity_selects_one_of_six_complete_public_fields() {
        let (xml, data) = multiple_fields();
        assert!(crate::wda_ui_admission::current_text_snapshot(&xml).is_none());
        assert_eq!(
            active_field_from_context(&data, &xml, "editor").unwrap(),
            ("editor".into(), "Alpha\n".into())
        );
        assert_eq!(
            active_field_from_context(&data, &xml, "field-3").unwrap(),
            ("field-3".into(), String::new())
        );
        for active in ["missing", "keyboard"] {
            assert!(active_field_from_context(&data, &xml, active).is_err());
        }
        assert!(active_field_from_context(&emoji_items(), EMOJI_XML, "emoji-search").is_err());
    }

    #[test]
    fn multiple_fields_require_exact_complete_native_context() {
        let (xml, data) = multiple_fields();
        let mut missing = data.clone();
        missing.as_array_mut().unwrap().pop();
        let mut duplicate = data.clone();
        duplicate.as_array_mut().unwrap().push(data[2].clone());
        for invalid in [missing, duplicate] {
            assert!(active_field_from_context(&invalid, &xml, "editor").is_err());
        }
        for (key, value) in [
            ("attribute/name", json!("other")),
            ("attribute/value", json!("changed")),
            ("attribute/focused", Value::Null),
            ("displayed", json!(false)),
            ("enabled", json!(false)),
            ("type", json!("XCUIElementTypeSecureTextField")),
            ("label", json!("Enter Passcode")),
            ("ELEMENT", json!("editor")),
            ("rect", json!({"x":10,"y":411,"width":200,"height":12})),
        ] {
            let mut changed = data.clone();
            changed[2][key] = value;
            assert!(
                active_field_from_context(&changed, &xml, "editor").is_err(),
                "{key}"
            );
        }
        let extra_xml = xml.replace(
            "</XCUIElementTypeApplication>",
            r#"<XCUIElementTypeTextField name="extra" visible="false" enabled="false" x="0" y="0" width="1" height="1"/></XCUIElementTypeApplication>"#,
        );
        assert!(active_field_from_context(&data, &extra_xml, "editor").is_err());
    }

    #[test]
    fn native_active_identity_never_admits_hidden_or_disabled_stale_fields() {
        let (xml, data) = multiple_fields();
        for (xml_key, native_key) in [("visible", "displayed"), ("enabled", "enabled")] {
            let xml = xml.replacen(
                &format!(r#"{xml_key}="true""#),
                &format!(r#"{xml_key}="false""#),
                1,
            );
            let mut data = data.clone();
            data[0][native_key] = json!(false);
            assert!(active_field_from_context(&data, &xml, "editor").is_err());
            assert!(active_field_from_context(&data, &xml, "field-3").is_ok());
        }
        let mut hidden_native = data;
        hidden_native[0]["displayed"] = json!(false);
        assert!(active_field_from_context(&hidden_native, &xml, "editor").is_err());
    }

    #[test]
    fn explicit_null_value_matches_a_guarded_empty_public_editor() {
        for kind in ["TextView", "TextField", "SearchField"] {
            for value_attribute in ["value=\"\"", ""] {
                let xml = XML
                    .replace("TextView", kind)
                    .replace("value=\"Alpha&#10;\"", value_attribute)
                    .replace(
                        &format!("XCUIElementType{kind} "),
                        &format!("XCUIElementType{kind} focused=\"true\" "),
                    );
                let snapshot = crate::wda_ui_admission::current_text_snapshot(&xml).unwrap();
                let mut data = items();
                data[0]["type"] = json!(format!("XCUIElementType{kind}"));
                data[0]["attribute/value"] = Value::Null;
                assert_eq!(
                    field_from_context(&data, &snapshot).unwrap(),
                    ("editor".into(), String::new()),
                    "{kind} {value_attribute}"
                );
            }
        }
    }

    #[test]
    fn null_value_keeps_context_and_attribute_refusals() {
        let xml = XML.replace("Alpha&#10;", "");
        let snapshot = crate::wda_ui_admission::current_text_snapshot(&xml).unwrap();
        let empty = || {
            let mut data = items();
            data[0]["attribute/value"] = Value::Null;
            data
        };
        for kind in ["SecureTextField", "Alert", "Sheet", "Other"] {
            let mut data = empty();
            data[0]["type"] = json!(format!("XCUIElementType{kind}"));
            assert!(field_from_context(&data, &snapshot).is_err(), "{kind}");
        }
        for kind in ["SecureTextField", "Alert", "Sheet"] {
            let mut data = empty();
            let mut hidden = element(kind, "hidden");
            hidden["displayed"] = json!(false);
            hidden["attribute/value"] = Value::Null;
            data.as_array_mut().unwrap().push(hidden);
            assert!(field_from_context(&data, &snapshot).is_err(), "{kind}");
        }
        for marker in MARKERS {
            let mut data = empty();
            data[1]["attribute/name"] = json!(marker);
            assert!(field_from_context(&data, &snapshot).is_err(), "{marker}");
        }
        for (key, bad) in [
            ("enabled", Value::Null),
            ("enabled", json!(false)),
            ("displayed", Value::Null),
            ("displayed", json!(false)),
            ("attribute/focused", Value::Null),
            ("attribute/focused", json!("unknown")),
            ("attribute/value", json!(true)),
            ("attribute/value", json!(0)),
            ("attribute/value", json!({})),
            ("attribute/value", json!([])),
        ] {
            let mut data = empty();
            data[0][key] = bad;
            assert!(field_from_context(&data, &snapshot).is_err(), "{key}");
        }
        for key in ["attribute/value", "attribute/focused"] {
            let mut data = empty();
            data[0].as_object_mut().unwrap().remove(key);
            assert!(field_from_context(&data, &snapshot).is_err(), "{key}");
        }
        assert!(
            classify(&empty()).is_err(),
            "nonempty baseline must disagree"
        );
    }

    #[test]
    fn rich_context_requires_one_matching_public_editor_and_keyboard() {
        assert_eq!(
            classify(&items()).unwrap(),
            ("editor".into(), "Alpha\n".into())
        );
        for kind in ["SecureTextField", "Alert", "Sheet"] {
            let mut data = items();
            let mut hidden = element(kind, "hidden");
            hidden["displayed"] = json!(false);
            data.as_array_mut().unwrap().push(hidden);
            assert!(classify(&data).is_err());
        }
        for key in [
            "label",
            "attribute/name",
            "attribute/value",
            "attribute/placeholderValue",
        ] {
            for marker in MARKERS {
                let mut data = items();
                for surrounded in [format!("prefix{marker}"), format!("{marker}suffix")] {
                    data[1][key] = json!(surrounded);
                    assert!(classify(&data).is_err());
                }
            }
        }
        for (key, value) in [
            ("attribute/value", json!("changed")),
            ("enabled", json!(false)),
            ("displayed", json!(false)),
            ("ELEMENT", json!("different")),
            ("rect", json!({"x":0,"y":100,"width":401,"height":300})),
            ("type", json!("XCUIElementTypeSearchField")),
        ] {
            let mut data = items();
            data[0][key] = value;
            assert!(classify(&data).is_err(), "{key}");
        }
        let mut duplicate = items();
        duplicate
            .as_array_mut()
            .unwrap()
            .push(element("TextView", "second"));
        assert!(classify(&duplicate).is_err());
        assert!(classify(&json!([element("TextView", "editor")])).is_err());
        for key in ["attribute/value", "attribute/focused", "attribute/name"] {
            let mut data = items();
            data[0].as_object_mut().unwrap().remove(key);
            assert!(classify(&data).is_err());
        }
    }

    #[test]
    fn baseline_extraction_keeps_complete_tree_refusals() {
        for bad in [
            XML.replace("Alpha&#10;", "Enter Passcode"),
            XML.replace("TextView", "SecureTextField"),
            XML.replace("value=", "focused=\"false\" value="),
            XML.replace("Alpha&#10;", "")
                .replace("value=", "focused=\"false\" value="),
            XML.replace("Alpha&#10;", "")
                .replace("value=", "focused=\"unknown\" value="),
            XML.replace(
                "</XCUIElementTypeApplication>",
                "<XCUIElementTypeTextField visible=\"true\" enabled=\"true\" x=\"0\" y=\"410\" width=\"400\" height=\"40\"/></XCUIElementTypeApplication>",
            ),
        ] {
            assert!(crate::wda_ui_admission::current_text_snapshot(&bad).is_none());
        }
    }

    struct FocusFake {
        mode: u8,
        sends: RefCell<usize>,
    }
    impl WdaRequest for FocusFake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let result = if method == "POST" {
                assert!(path.ends_with("/wda/keys"));
                *self.sends.borrow_mut() += 1;
                Value::Null
            } else if path.ends_with("/element/active") {
                match self.mode {
                    1 => json!({"ELEMENT":"another-editor"}),
                    4 => json!({}),
                    5 => json!({"ELEMENT":"emoji-search"}),
                    _ => json!({"ELEMENT":"editor"}),
                }
            } else if path.ends_with("/wda/activeAppInfo") {
                json!({"bundleId":if self.mode==2 { "other.app" } else { "example.editor" }})
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
    async fn changed_or_unknown_focus_app_or_lock_never_sends_text() {
        let xml = XML.replace("Alpha&#10;", "");
        let snapshot = crate::wda_ui_admission::current_text_snapshot(&xml).unwrap();
        let mut data = items();
        data[0]["attribute/value"] = Value::Null;
        let (field, before) = field_from_context(&data, &snapshot).unwrap();
        assert!(before.is_empty());
        for mode in 0..=4 {
            let fake = FocusFake {
                mode,
                sends: RefCell::new(0),
            };
            let result = dispatch_to_focused(
                &fake,
                "session",
                &field,
                "example.editor",
                &Action::TextCurrent { value: "X".into() },
                None,
            )
            .await;
            assert_eq!(result.is_ok(), mode == 0);
            assert_eq!(*fake.sends.borrow(), usize::from(mode == 0));
        }
    }

    #[tokio::test]
    async fn emoji_search_never_becomes_the_typing_recipient() {
        let snapshot = crate::wda_ui_admission::current_text_snapshot(EMOJI_XML).unwrap();
        let (field, _) = field_from_context(&emoji_items(), &snapshot).unwrap();
        for mode in [0, 5] {
            let fake = FocusFake {
                mode,
                sends: RefCell::new(0),
            };
            let result = dispatch_to_focused(
                &fake,
                "session",
                &field,
                "example.editor",
                &Action::TextCurrent { value: "X".into() },
                None,
            )
            .await;
            assert_eq!(result.is_ok(), mode == 0);
            assert_eq!(*fake.sends.borrow(), usize::from(mode == 0));
        }
    }

    struct SettingsFake {
        pair: RefCell<Value>,
        writes: RefCell<Vec<Value>>,
        mode: u8,
    }
    impl SettingsFake {
        fn new(mode: u8) -> Self {
            Self {
                pair: RefCell::new(
                    json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"name,label"}),
                ),
                writes: RefCell::new(vec![]),
                mode,
            }
        }
    }
    impl WdaRequest for SettingsFake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let mut status = 200;
            let response = if path.ends_with("/appium/settings") {
                if method == "POST" {
                    let pair: Value = serde_json::from_slice(body).unwrap();
                    assert_eq!(pair["settings"].as_object().unwrap().len(), 2);
                    self.writes.borrow_mut().push(pair["settings"].clone());
                    *self.pair.borrow_mut() = pair["settings"].clone();
                    if (self.mode == 2 && self.writes.borrow().len() == 1)
                        || (self.mode == 3 && self.writes.borrow().len() == 2)
                    {
                        status = 500;
                    }
                }
                self.pair.borrow().clone()
            } else {
                assert_eq!(method, "POST");
                assert!(path.ends_with("/elements"));
                let query: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(query["using"], "predicate string");
                for marker in MARKERS {
                    for attr in ["name", "label", "value", "placeholderValue"] {
                        assert!(
                            query["value"]
                                .as_str()
                                .unwrap()
                                .contains(&format!("{attr} CONTAINS '{marker}'"))
                        );
                    }
                }
                if self.mode == 1 {
                    status = 500;
                }
                if self.mode == 4 {
                    *self.pair.borrow_mut() = json!({"shouldUseCompactResponses":false,"elementResponseAttributes":"external"});
                }
                items()
            };
            Ok(HttpResponse {
                status,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":response})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn format_transaction_restores_after_query_or_set_failure_without_retry() {
        for mode in 0..=3 {
            let fake = SettingsFake::new(mode);
            let original = fake.pair.borrow().clone();
            let result = context(&fake, "session").await;
            assert_eq!(*fake.pair.borrow(), original);
            assert_eq!(fake.writes.borrow().len(), 2);
            match mode {
                0 => assert!(result.unwrap().is_ok()),
                1 | 2 => assert!(result.unwrap().is_err()),
                3 => assert!(result.is_err()),
                _ => unreachable!(),
            }
        }
        let fake = SettingsFake::new(4);
        assert!(context(&fake, "session").await.is_err());
        assert_eq!(
            fake.writes.borrow().len(),
            1,
            "must not overwrite another writer"
        );
    }

    struct AppContextFake {
        settings: SettingsFake,
        context_started: tokio::sync::Notify,
        app_matches: bool,
    }

    impl WdaRequest for AppContextFake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let result = if path.ends_with("/wda/activeAppInfo") {
                self.context_started.notified().await;
                json!({"bundleId": if self.app_matches { "example.editor" } else { "example.other" }})
            } else if path.ends_with("/wda/locked") {
                json!(false)
            } else if path.ends_with("/element/active") {
                json!({"ELEMENT":"editor"})
            } else {
                self.context_started.notify_one();
                return self.settings.request(method, path, body, timeout).await;
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":result})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn context_and_app_check_overlap_without_skipping_restoration() {
        for (app_matches, settings_mode) in [(true, 0), (false, 0), (false, 3)] {
            let fake = AppContextFake {
                settings: SettingsFake::new(settings_mode),
                context_started: tokio::sync::Notify::new(),
                app_matches,
            };
            let original = fake.settings.pair.borrow().clone();
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                context_in_app(&fake, "session", "example.editor"),
            )
            .await
            .expect("context must start while the app check is pending");
            assert_eq!(*fake.settings.pair.borrow(), original);
            assert_eq!(fake.settings.writes.borrow().len(), 2);
            if settings_mode == 3 {
                assert!(
                    result.is_err(),
                    "restore failure must retain poison precedence"
                );
            } else {
                assert_eq!(result.unwrap().is_ok(), app_matches);
            }
        }
    }

    #[test]
    fn readback_proves_only_one_current_selection_insertion_or_replacement() {
        for (before, text, after) in [
            ("Alpha\n", "X", "AlphaX\n"),
            ("Alpha\n", "new", "new\n"),
            ("aab", "aa", "aaab"),
            ("\u{732b}\u{72ac}", "\u{72d0}", "\u{732b}\u{72d0}\u{72ac}"),
            ("old", "new", "new"),
        ] {
            assert!(inserted(before, text, after));
        }
        for (before, text, after) in [
            ("Alpha", "X", "Alpha"),
            ("Alpha", "X", "Xelsewhere"),
            ("secret", "", "secret"),
            ("a", "X", "XXa"),
            ("\u{732b}\u{72ac}", "X", "\u{732b}X\u{72d7}"),
        ] {
            assert!(!inserted(before, text, after));
        }
    }

    struct ValueFake(Value);

    impl WdaRequest for ValueFake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            assert_eq!(method, "GET");
            assert_eq!(path, "/session/session/element/editor/attribute/value");
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&self.0).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn empty_editor_readback_requires_exact_typed_string() {
        let mut replies = vec![(json!({"sessionId":"session"}), false)];
        for after in [
            json!("typed"),
            json!(""),
            json!("wrong"),
            json!("typed-extra"),
            json!("typedtyped"),
            Value::Null,
            json!(0),
            json!({}),
        ] {
            let accepted = after == "typed";
            replies.push((json!({"sessionId":"session","value":after}), accepted));
        }
        for (reply, accepted) in replies {
            let after = value(
                &ValueFake(reply),
                "session",
                "GET",
                "/session/session/element/editor/attribute/value",
                b"",
            )
            .await;
            assert_eq!(
                after.is_ok_and(|after| inserted_value("", "typed", &after)),
                accepted
            );
        }
    }
}

#[cfg(test)]
mod restoration_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        settings: RefCell<Value>,
        fail_restore: bool,
        writes: Cell<u8>,
    }
    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            _path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            if method == "POST" {
                let next: Value = serde_json::from_slice(body).unwrap();
                self.writes.set(self.writes.get() + 1);
                if !(self.fail_restore && self.writes.get() > 1) {
                    *self.settings.borrow_mut() = next["settings"].clone();
                }
            }
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(
                    &json!({"sessionId":"session","value":self.settings.borrow().clone()}),
                )
                .unwrap(),
            })
        }
    }
    #[tokio::test(start_paused = true)]
    async fn cancelled_work_restores_settings_outside_capture_deadline() {
        let original = json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"type"});
        let fake = Fake {
            settings: RefCell::new(original.clone()),
            fail_restore: false,
            writes: Cell::new(0),
        };
        let result = rich_context(&fake, "session", ATTRIBUTES, async {
            std::future::pending::<Result<(), AdapterError>>().await
        })
        .await;
        assert!(result.unwrap().is_err());
        assert_eq!(*fake.settings.borrow(), original);
        assert_eq!(fake.writes.get(), 2);
    }
    #[tokio::test]
    async fn restoration_failure_is_distinct_from_work_refusal() {
        let fake = Fake {
            settings: RefCell::new(
                json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"type"}),
            ),
            fail_restore: true,
            writes: Cell::new(0),
        };
        assert!(
            rich_context(&fake, "session", ATTRIBUTES, async { Ok(()) })
                .await
                .is_err()
        );
    }
}
