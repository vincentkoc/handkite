use crate::wda_action::WdaRequest;
use handkite_core::controller::AdapterError;
use handkite_core::protocol::{ButtonTarget, TransportReceipt};
use serde_json::{Value, json};
use std::time::Duration;

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

pub(crate) struct ResolvedButton {
    path: String,
    session: String,
}

impl ResolvedButton {
    pub(crate) async fn click(
        self,
        wda: &impl WdaRequest,
    ) -> Result<TransportReceipt, AdapterError> {
        // XCTest chooses this element's hit point. Never fall back to coordinates.
        value(
            wda,
            &self.session,
            "POST",
            &format!("{}/click", self.path),
            b"{}",
        )
        .await?;
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: "click_button".into(),
        })
    }
}

pub(crate) async fn resolve(
    wda: &impl WdaRequest,
    session: &str,
    target: &ButtonTarget,
) -> Result<ResolvedButton, AdapterError> {
    if !target.valid() || !crate::wda_action::valid_session_id(session) {
        return Err(AdapterError);
    }
    // JSON string escaping also quotes backslashes and quotes in the predicate
    // literal. The caller never supplies predicate syntax or an element ID.
    let label = serde_json::to_string(&target.label).map_err(|_| AdapterError)?;
    let query = serde_json::to_vec(&json!({
        "using": "predicate string",
        "value": format!("type == 'XCUIElementTypeButton' AND label == {label}")
    }))
    .map_err(|_| AdapterError)?;
    let matches = value(
        wda,
        session,
        "POST",
        &format!("/session/{session}/elements"),
        &query,
    )
    .await?;
    let matches = matches
        .as_array()
        .filter(|items| items.len() == 1)
        .ok_or(AdapterError)?;
    let element = &matches[0];
    let read_id = |key| match element.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|id| crate::wda_action::valid_session_id(id))
            .map(Some)
            .ok_or(AdapterError),
    };
    let legacy = read_id("ELEMENT")?;
    let modern = read_id("element-6066-11e4-a52e-4f735466cecf")?;
    if legacy.zip(modern).is_some_and(|(a, b)| a != b) {
        return Err(AdapterError);
    }
    let id = modern.or(legacy).ok_or(AdapterError)?;
    let base = format!("/session/{session}/element/{id}");
    let paths = [
        format!("{base}/name"),
        format!("{base}/attribute/label"),
        format!("{base}/rect"),
        format!("{base}/enabled"),
        format!("{base}/displayed"),
    ];
    let (kind, label, rect, enabled, visible) = tokio::join!(
        value(wda, session, "GET", &paths[0], b""),
        value(wda, session, "GET", &paths[1], b""),
        value(wda, session, "GET", &paths[2], b""),
        value(wda, session, "GET", &paths[3], b""),
        value(wda, session, "GET", &paths[4], b""),
    );
    let rect = rect?;
    if kind? != "XCUIElementTypeButton"
        || label? != target.label
        || enabled? != true
        || visible? != true
        || rect["x"].as_f64() != Some(target.x)
        || rect["y"].as_f64() != Some(target.y)
        || rect["width"].as_f64() != Some(target.width)
        || rect["height"].as_f64() != Some(target.height)
    {
        return Err(AdapterError);
    }
    Ok(ResolvedButton {
        path: base,
        session: session.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wda_http::HttpResponse;
    use crate::wda_transport::WdaError;
    use std::cell::RefCell;

    struct Fake {
        changes: std::collections::BTreeMap<String, Value>,
        calls: RefCell<Vec<(String, String, Value)>>,
        fail_click: bool,
        wrong_session_at: Option<String>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                changes: Default::default(),
                calls: Default::default(),
                fail_click: false,
                wrong_session_at: None,
            }
        }
        fn clicks(&self) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|(_, path, _)| path.ends_with("/click"))
                .count()
        }
    }

    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            let body = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(body).unwrap()
            };
            self.calls
                .borrow_mut()
                .push((method.into(), path.into(), body));
            let default = if path.ends_with("/elements") {
                json!([{"ELEMENT":"button-1"}])
            } else if path.ends_with("/name") {
                json!("XCUIElementTypeButton")
            } else if path.ends_with("/label") {
                json!("Close")
            } else if path.ends_with("/rect") {
                json!({"x":338,"y":489,"width":38,"height":38})
            } else if path.ends_with("/enabled") || path.ends_with("/displayed") {
                json!(true)
            } else if path.ends_with("/click") {
                Value::Null
            } else {
                panic!("unexpected endpoint")
            };
            let value = self
                .changes
                .iter()
                .find(|(suffix, _)| path.ends_with(suffix.as_str()))
                .map(|(_, v)| v.clone())
                .unwrap_or(default);
            Ok(HttpResponse {
                status: if self.fail_click && path.ends_with("/click") {
                    500
                } else {
                    200
                },
                body: serde_json::to_vec(&json!({"value":value,"sessionId":if self.wrong_session_at.as_ref().is_some_and(|suffix|path.ends_with(suffix)){"other-session"}else{"session-1"}})).unwrap(),
            })
        }
    }

    fn target() -> ButtonTarget {
        ButtonTarget {
            label: "Close".into(),
            x: 338.0,
            y: 489.0,
            width: 38.0,
            height: 38.0,
        }
    }

    #[tokio::test]
    async fn resolves_validates_and_clicks_one_bound_identity() {
        let fake = Fake::new();
        let resolved = resolve(&fake, "session-1", &target()).await.unwrap();
        assert_eq!(fake.clicks(), 0);
        let receipt = resolved.click(&fake).await.unwrap();
        assert_eq!(receipt.accepted, Some(true));
        assert_eq!(fake.clicks(), 1);
        assert_eq!(
            fake.calls.borrow().last().unwrap().1,
            "/session/session-1/element/button-1/click"
        );
    }

    #[tokio::test]
    async fn ambiguous_missing_or_untrusted_identity_never_clicks() {
        for matched in [
            json!([]),
            json!([{"ELEMENT":"a"},{"ELEMENT":"b"}]),
            json!([{}]),
            json!([{"ELEMENT":"../bad"}]),
            json!([{"ELEMENT":null,"element-6066-11e4-a52e-4f735466cecf":"b"}]),
            json!([{"ELEMENT":"a","element-6066-11e4-a52e-4f735466cecf":42}]),
            json!([{"ELEMENT":"a","element-6066-11e4-a52e-4f735466cecf":"b"}]),
        ] {
            let mut fake = Fake::new();
            fake.changes.insert("/elements".into(), matched);
            assert!(resolve(&fake, "session-1", &target()).await.is_err());
            assert_eq!(fake.clicks(), 0);
            assert_eq!(fake.calls.borrow().len(), 1);
        }
    }

    #[tokio::test]
    async fn changed_attributes_or_error_envelope_never_click() {
        for (suffix, bad) in [
            ("/name", json!("XCUIElementTypeTextField")),
            ("/label", json!("Other")),
            ("/enabled", json!(false)),
            ("/displayed", json!(false)),
            ("/rect", json!({"x":339,"y":489,"width":38,"height":38})),
            ("/rect", json!({"error":"stale element reference"})),
        ] {
            let mut fake = Fake::new();
            fake.changes.insert(suffix.into(), bad);
            assert!(
                resolve(&fake, "session-1", &target()).await.is_err(),
                "{suffix}"
            );
            assert_eq!(fake.clicks(), 0);
        }
    }

    #[tokio::test]
    async fn session_drift_in_query_or_attributes_never_clicks() {
        for suffix in ["/elements", "/label", "/rect"] {
            let mut fake = Fake::new();
            fake.wrong_session_at = Some(suffix.into());
            assert!(resolve(&fake, "session-1", &target()).await.is_err());
            assert_eq!(fake.clicks(), 0);
        }
    }

    #[tokio::test]
    async fn failed_click_is_not_retried_or_changed_to_coordinate_tap() {
        let mut fake = Fake::new();
        fake.fail_click = true;
        assert!(
            resolve(&fake, "session-1", &target())
                .await
                .unwrap()
                .click(&fake)
                .await
                .is_err()
        );
        assert_eq!(fake.clicks(), 1);
        assert!(
            !fake
                .calls
                .borrow()
                .iter()
                .any(|(_, p, _)| p.ends_with("/actions") || p.ends_with("/wda/tap"))
        );
    }

    #[tokio::test]
    async fn predicate_literal_cannot_inject_an_extra_match() {
        let fake = Fake::new();
        let mut target = target();
        target.label = "x\" OR TRUEPREDICATE OR label == \"x".into();
        assert!(resolve(&fake, "session-1", &target).await.is_err());
        let query = &fake.calls.borrow()[0].2["value"];
        assert_eq!(
            query.as_str().unwrap(),
            format!(
                "type == 'XCUIElementTypeButton' AND label == {}",
                serde_json::to_string(&target.label).unwrap()
            )
        );
        assert_eq!(fake.clicks(), 0);
    }
}
