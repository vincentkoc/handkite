use super::*;
use crate::wda_ui_admission::Attributes;
use serde_json::{Value, json};

const ATTRIBUTES: &str = "type,enabled,displayed,rect,label,attribute/name,attribute/value,attribute/placeholderValue,attribute/focused";
const MARKERS: [&str; 4] = [
    "proud-lock",
    "passcode-view",
    "call-to-action-label",
    "Enter Passcode",
];

pub(super) struct Input {
    token: u64,
    key: cache::Key,
    field: String,
    before: String,
    text: String,
    app: String,
    until: Instant,
    sent: bool,
}

async fn value(
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

fn id(item: &Value) -> Result<String, AdapterError> {
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
    let path = format!("/session/{session}/appium/settings");
    let original = settings_pair(&value(wda, session, "GET", &path, b"").await?)?;
    if settings_pair(&value(wda, session, "GET", &path, b"").await?)? != original {
        return Err(AdapterError);
    }
    let target = json!({"shouldUseCompactResponses":false,"elementResponseAttributes":ATTRIBUTES});
    let set = serde_json::to_vec(&json!({"settings":target})).map_err(|_| AdapterError)?;
    let result = async {
        value(wda, session, "POST", &path, &set).await?;
        if settings_pair(&value(wda, session, "GET", &path, b"").await?)? != target {
            return Err(AdapterError);
        }
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
        let query =
            serde_json::to_vec(&json!({"using":"predicate string", "value":terms.join(" OR ")}))
                .map_err(|_| AdapterError)?;
        let _query = crate::timing::Span::new("text_context_query");
        let items = value(
            wda,
            session,
            "POST",
            &format!("/session/{session}/elements"),
            &query,
        )
        .await?;
        Ok((items, Instant::now(), unix_ms()?))
    }
    .await;
    // One restoration attempt after every attempted set, including an unknown ACK.
    let current = settings_pair(&value(wda, session, "GET", &path, b"").await?)?;
    if current != original {
        if current != target {
            return Err(AdapterError);
        }
        let restore =
            serde_json::to_vec(&json!({"settings":original})).map_err(|_| AdapterError)?;
        let receipt = value(wda, session, "POST", &path, &restore).await;
        let readback = settings_pair(&value(wda, session, "GET", &path, b"").await?)?;
        if readback != original {
            return Err(AdapterError);
        }
        receipt?;
    }
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
    screen: &Attributes,
    baseline: &Attributes,
) -> Result<(String, String), AdapterError> {
    let screen = rect(screen).ok_or(AdapterError)?;
    let items = items
        .as_array()
        .filter(|items| !items.is_empty() && items.len() <= 64)
        .ok_or(AdapterError)?;
    let mut ids = std::collections::BTreeSet::new();
    let mut field = None;
    let mut keyboards = 0;
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
        let displayed = item["displayed"].as_bool().ok_or(AdapterError)?;
        let enabled = item["enabled"].as_bool().ok_or(AdapterError)?;
        // This attribute is false on iOS even for /element/active. The active
        // endpoint's identity is checked immediately before input instead.
        item["attribute/focused"].as_bool().ok_or(AdapterError)?;
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
            keyboards += 1;
            continue;
        }
        if field.is_some() || rect(baseline) != Some(bounds) {
            return Err(AdapterError);
        }
        for key in ["type", "name", "label", "value", "placeholderValue"] {
            if attrs.get(key) != baseline.get(key) {
                return Err(AdapterError);
            }
        }
        field = Some((identity, attrs.get("value").cloned().ok_or(AdapterError)?));
    }
    if keyboards != 1 {
        return Err(AdapterError);
    }
    field.ok_or(AdapterError)
}

async fn active(wda: &impl WdaRequest, session: &str) -> Result<String, AdapterError> {
    id(&value(
        wda,
        session,
        "GET",
        &format!("/session/{session}/element/active"),
        b"",
    )
    .await?)
}

async fn app_matches(wda: &impl WdaRequest, session: &str, app: &str) -> Result<(), AdapterError> {
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
        let Some((screen, previous)) =
            crate::wda_ui_admission::current_text_snapshot(&baseline.accessibility_tree)
        else {
            return Ok(None);
        };
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
            .block_on(async {
                if let Err(error) = app_matches(&wda, &key.session, app).await {
                    return Ok(Err(error));
                }
                context(&wda, &key.session).await
            });
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
        let (items, captured_at, finished_unix_ms) = match query {
            Ok(query) => query,
            Err(_) => return Ok(None),
        };
        let (field, before) = match field_from_context(&items, &screen, &previous) {
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
                let _focus = crate::timing::Span::new("text_final_focus");
                let (active, app) = tokio::join!(
                    active(&wda, &input.key.session),
                    app_matches(&wda, &input.key.session, &input.app)
                );
                if active.map_err(|_| AdapterDiagnostic::PreActionCapture)? != input.field
                    || app.is_err()
                {
                    return Err(AdapterDiagnostic::PreActionCapture);
                }
                drop(_focus);
                WdaActions::execute_on_diagnostic(&wda, action, &input.key.session).await
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
                        app.is_ok()
                            && after?
                                .as_str()
                                .is_some_and(|after| inserted(&input.before, &input.text, after)),
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
        let (screen, field) = crate::wda_ui_admission::current_text_snapshot(XML).unwrap();
        field_from_context(items, &screen, &field)
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
                data[1][key] = json!(marker);
                assert!(classify(&data).is_err());
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
        ] {
            assert!(crate::wda_ui_admission::current_text_snapshot(&bad).is_none());
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
                    assert!(query["value"].as_str().unwrap().contains(marker));
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
}
