use super::*;
use serde_json::Value;

async fn read(wda: &impl WdaRequest, path: &str) -> Result<Value, AdapterError> {
    response_value(
        wda.request("GET", path, b"", Duration::from_secs(5))
            .await
            .map_err(|_| AdapterError)?,
    )
}

fn scale(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite() && *v > 0.)
}

fn selected_scale(displays: &Value, selected: u64) -> Option<f64> {
    let displays = displays.as_array()?;
    let mut found = displays
        .iter()
        .filter(|display| display["displayId"].as_u64() == Some(selected));
    let result = scale(&found.next()?["scale"])?;
    found.next().is_none().then_some(result)
}

pub(super) async fn selection(
    wda: &impl WdaRequest,
    session: &str,
) -> Result<Option<u64>, AdapterError> {
    let path = format!("/session/{session}/appium/settings");
    Ok(
        super::focused_text::value(wda, session, "GET", &path, b"").await?["currentDisplayId"]
            .as_u64(),
    )
}

pub(super) async fn verify_selection(
    wda: &impl WdaRequest,
    session: &str,
    expected: Option<u64>,
) -> Result<(), AdapterError> {
    (selection(wda, session).await? == expected)
        .then_some(())
        .ok_or(AdapterError)
}

pub(super) async fn screen_scale(
    wda: &impl WdaRequest,
    selected: Option<u64>,
) -> Result<f64, AdapterError> {
    if let Some(selected) = selected {
        let displays = read(wda, "/wda/screens").await;
        if let Some(scale) = displays.ok().and_then(|v| selected_scale(&v, selected)) {
            return Ok(scale);
        }
    }
    // Older runners do not expose the selected display inventory. Retain their
    // original endpoint; callers still verify both images against the viewport.
    let screen = read(wda, "/wda/screen").await?;
    if selected.is_some() && screen["displayId"].as_u64() != selected {
        return Err(AdapterError);
    }
    scale(&screen["scale"]).ok_or(AdapterError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct Fake(RefCell<VecDeque<(&'static str, Value)>>);
    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            assert_eq!(method, "GET");
            let (expected, value) = self.0.borrow_mut().pop_front().expect("unexpected request");
            assert_eq!(path, expected);
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"test", "value":value})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn selected_display_uses_inventory_without_app_snapshot() {
        let fake = Fake(RefCell::new(VecDeque::from([
            (
                "/session/test/appium/settings",
                json!({"currentDisplayId":7}),
            ),
            (
                "/wda/screens",
                json!([{"displayId":1,"scale":2}, {"displayId":7,"scale":3}]),
            ),
            (
                "/session/test/appium/settings",
                json!({"currentDisplayId":7}),
            ),
        ])));
        let selected = selection(&fake, "test").await.unwrap();
        assert_eq!(screen_scale(&fake, selected).await.unwrap(), 3.);
        verify_selection(&fake, "test", selected).await.unwrap();
        assert!(fake.0.borrow().is_empty());
    }

    #[tokio::test]
    async fn ambiguous_missing_and_malformed_inventory_keep_legacy_mapping() {
        for inventory in [
            json!([]),
            json!({}),
            json!([{"displayId":1,"scale":3}]),
            json!([{"displayId":7,"scale":3},{"displayId":7,"scale":3}]),
            json!([{"displayId":7,"scale":0}]),
            json!([{"displayId":7,"scale":"3"}]),
        ] {
            let fake = Fake(RefCell::new(VecDeque::from([
                ("/wda/screens", inventory),
                ("/wda/screen", json!({"displayId":7,"scale":2})),
            ])));
            assert_eq!(screen_scale(&fake, Some(7)).await.unwrap(), 2.);
            assert!(fake.0.borrow().is_empty());
        }
        let fake = Fake(RefCell::new(VecDeque::from([(
            "/wda/screen",
            json!({"scale":2}),
        )])));
        assert_eq!(screen_scale(&fake, None).await.unwrap(), 2.);
    }

    #[tokio::test]
    async fn selection_change_or_wrong_fallback_refuses() {
        let fake = Fake(RefCell::new(VecDeque::from([(
            "/session/test/appium/settings",
            json!({"currentDisplayId":2}),
        )])));
        assert!(verify_selection(&fake, "test", Some(1)).await.is_err());
        let fake = Fake(RefCell::new(VecDeque::from([
            ("/wda/screens", json!([])),
            ("/wda/screen", json!({"displayId":2,"scale":3})),
        ])));
        assert!(screen_scale(&fake, Some(1)).await.is_err());
    }

    #[tokio::test]
    async fn settings_from_another_session_are_rejected() {
        let fake = Fake(RefCell::new(VecDeque::from([(
            "/session/other/appium/settings",
            json!({"currentDisplayId":1}),
        )])));
        assert!(selection(&fake, "other").await.is_err());
    }
}
