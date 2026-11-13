use crate::wda_action::WdaRequest;
use crate::wda_http::HttpResponse;
use crate::wda_settings::validate_session;
use handkite_core::passcode::PasscodeProgress;
use serde::Deserialize;
use std::time::Duration;

pub const QUERY: &[u8] = br#"{"using":"class name","value":"XCUIElementTypeSecureTextField"}"#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldError {
    Response,
    Session,
    Element,
    Progress,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    value: T,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    status: Option<u16>,
}

pub fn value<T: serde::de::DeserializeOwned>(
    response: HttpResponse,
    session: &str,
) -> Result<T, FieldError> {
    if response.status != 200 || response.body.len() > 4 * 1024 * 1024 {
        return Err(FieldError::Response);
    }
    let parsed: Envelope<T> =
        serde_json::from_slice(&response.body).map_err(|_| FieldError::Response)?;
    if parsed.status.is_some_and(|status| status != 0) {
        return Err(FieldError::Response);
    }
    if parsed.session.as_deref().is_some_and(|id| id != session) {
        return Err(FieldError::Session);
    }
    Ok(parsed.value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Element {
    #[serde(rename = "ELEMENT")]
    legacy: Option<String>,
    #[serde(rename = "element-6066-11e4-a52e-4f735466cecf")]
    w3c: Option<String>,
}

pub fn single_element(response: HttpResponse, session: &str) -> Result<String, FieldError> {
    let mut elements: Vec<Element> = value(response, session)?;
    if elements.len() != 1 {
        return Err(FieldError::Element);
    }
    let element = elements.pop().ok_or(FieldError::Element)?;
    let id = match (element.legacy, element.w3c) {
        (Some(left), Some(right)) if left == right => left,
        (Some(id), None) | (None, Some(id)) => id,
        _ => return Err(FieldError::Element),
    };
    if !valid_id(&id) {
        return Err(FieldError::Element);
    }
    Ok(id)
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub fn count(raw: &str) -> Result<u32, FieldError> {
    if raw.len() > 256 {
        return Err(FieldError::Progress);
    }
    PasscodeProgress::parse(raw, 6)
        .map(|progress| progress.entered)
        .map_err(|_| FieldError::Progress)
}

pub(crate) async fn read(wda: &impl WdaRequest, session: &str) -> Result<u32, FieldError> {
    read_with(wda, session).await
}

async fn read_with(wda: &impl WdaRequest, session: &str) -> Result<u32, FieldError> {
    validate_session(wda, session)
        .await
        .map_err(|_| FieldError::Session)?;
    let response = wda
        .request(
            "POST",
            &format!("/session/{session}/elements"),
            QUERY,
            Duration::from_secs(10),
        )
        .await
        .map_err(|_| FieldError::Response)?;
    let element = single_element(response, session)?;
    let response = wda
        .request(
            "GET",
            &format!("/session/{session}/element/{element}/attribute/value"),
            b"",
            Duration::from_secs(10),
        )
        .await
        .map_err(|_| FieldError::Response)?;
    let raw: String = value(response, session)?;
    let count = count(&raw)?;
    validate_session(wda, session)
        .await
        .map_err(|_| FieldError::Session)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_and_element_ids_share_bounded_ascii_validation() {
        for id in ["fixture-session", "A0-b9", &"x".repeat(128)] {
            assert!(valid_id(id));
        }
        for id in ["", "../session", "a/b", "a_b", "é", &"x".repeat(129)] {
            assert!(!valid_id(id));
        }
    }
    use crate::wda_transport::WdaError;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    struct Step {
        method: &'static str,
        path: &'static str,
        response: Result<HttpResponse, WdaError>,
    }
    struct Script {
        steps: RefCell<VecDeque<Step>>,
        queries: Cell<usize>,
        calls: Cell<usize>,
    }
    fn response(method: &'static str, path: &'static str, body: serde_json::Value) -> Step {
        Step {
            method,
            path,
            response: Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            }),
        }
    }
    fn status(id: &str) -> Step {
        response(
            "GET",
            "/status",
            json!({"sessionId":id,"value":{"ready":true}}),
        )
    }
    fn script() -> Script {
        Script {
            steps: RefCell::new(
                vec![
                    status("fixture-session"),
                    response("GET", "/session/fixture-session", json!({"value":{}})),
                    response(
                        "POST",
                        "/session/fixture-session/elements",
                        json!({"value":[{"ELEMENT":"fixture-element"}]}),
                    ),
                    response(
                        "GET",
                        "/session/fixture-session/element/fixture-element/attribute/value",
                        json!({"value":"1 of 6 digits"}),
                    ),
                    status("fixture-session"),
                    response("GET", "/session/fixture-session", json!({"value":{}})),
                ]
                .into(),
            ),
            queries: Cell::new(0),
            calls: Cell::new(0),
        }
    }
    impl WdaRequest for Script {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            let step = self
                .steps
                .borrow_mut()
                .pop_front()
                .expect("no retry or extra request");
            assert_eq!((method, path), (step.method, step.path));
            if method == "POST" {
                assert_eq!(body, QUERY);
                self.queries.set(self.queries.get() + 1);
            } else {
                assert!(body.is_empty());
            }
            self.calls.set(self.calls.get() + 1);
            step.response
        }
    }
    #[tokio::test]
    async fn one_lookup_returns_only_a_count_after_same_session_checks() {
        let wda = script();
        assert_eq!(read_with(&wda, "fixture-session").await, Ok(1));
        assert_eq!(wda.queries.get(), 1);
        assert!(wda.steps.borrow().is_empty());
    }
    #[tokio::test]
    async fn session_drift_and_read_failures_stop_without_repeating_the_lookup() {
        for index in [0, 4] {
            let wda = script();
            wda.steps.borrow_mut()[index] = status("changed-session");
            assert_eq!(
                read_with(&wda, "fixture-session").await,
                Err(FieldError::Session)
            );
            assert_eq!(wda.queries.get(), usize::from(index == 4));
            assert_eq!(wda.calls.get(), index + 1);
        }
        for index in [2, 3] {
            let wda = script();
            wda.steps.borrow_mut()[index].response = Err(WdaError::AfterSent);
            assert_eq!(
                read_with(&wda, "fixture-session").await,
                Err(FieldError::Response)
            );
            assert_eq!(wda.queries.get(), 1);
            assert_eq!(wda.calls.get(), index + 1);
        }
    }
}
