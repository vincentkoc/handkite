//! One explicitly admitted WDA session creation, never adoption or renewal.

use crate::wda_action::{WdaRequest, valid_session_id};
use crate::wda_http::HttpResponse;
use crate::wda_settings::{self, IdleSettingsOutcome};
use crate::wda_transport::{UsbWda, WdaError};
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::future::Future;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);
const CREATE: &[u8] = br#"{"capabilities":{"alwaysMatch":{}}}"#;
const CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// Acknowledgement is not proof that the device-side runner exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionClose {
    Acknowledged,
    Refused,
    Unknown,
}

/// Attempt one bounded DELETE for the exact session created by this owner.
/// A missing or drifted session refuses deletion; an uncertain DELETE is never retried.
pub async fn close_owned_session(wda: &UsbWda, session: &str) -> SessionClose {
    let deadline = Instant::now() + CLOSE_BUDGET;
    let guarded = wda.with_write_deadline(deadline);
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        close_session(&guarded, session),
    )
    .await
    .unwrap_or(SessionClose::Unknown)
}

async fn close_session(wda: &impl WdaRequest, session: &str) -> SessionClose {
    if !valid_session_id(session) {
        return SessionClose::Refused;
    }
    let guarded = OwnedRequests {
        wda,
        session,
        stopped: Cell::new(false),
    };
    if guarded.validate().await.is_err() {
        return SessionClose::Refused;
    }
    let result = wda
        .request("DELETE", &format!("/session/{session}"), b"", DEADLINE)
        .await;
    let Ok(response) = result else {
        return SessionClose::Unknown;
    };
    if response.status != 200 {
        return SessionClose::Unknown;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&response.body) else {
        return SessionClose::Unknown;
    };
    if value.get("value") != Some(&Value::Null)
        || value.get("error").is_some()
        || value
            .get("status")
            .is_some_and(|status| status.as_u64() != Some(0))
        || value
            .get("sessionId")
            .is_some_and(|id| !id.is_null() && id.as_str() != Some(session))
    {
        return SessionClose::Unknown;
    }
    SessionClose::Acknowledged
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedSessionError {
    AdmissionRefused,
    PreflightUnproven,
    CreationUnknown,
    SessionUnproven,
}

impl OwnedSessionError {
    pub fn message(self) -> &'static str {
        match self {
            Self::AdmissionRefused => "refused reason=session_admission",
            Self::PreflightUnproven => "refused reason=session_preflight",
            Self::CreationUnknown => "unknown reason=session_creation_unproven",
            Self::SessionUnproven => "unknown reason=owned_session_unproven",
        }
    }
}

/// Creates and validates one new session without changing settings or running input.
/// Requires a reviewed handoff from every other WDA owner for the whole call.
/// `admit` runs before preflight and immediately before the sole creation POST;
/// unlock callers must check the PIN stop gate there.
///
/// Returns only the new receipt's exact ID after ready-status and session-endpoint
/// validation. No request is retried, an uncertain receipt never falls back to
/// discovery or adoption, and the session is retained for explicit owner handoff.
pub async fn create_owned_session(
    wda: &UsbWda,
    admit: impl FnMut() -> Result<(), ()>,
) -> Result<String, OwnedSessionError> {
    create_session(wda, admit).await
}

pub(crate) async fn start_owned_idle_settings(
    wda: &impl WdaRequest,
    session: &str,
) -> Result<wda_settings::IdleSettingsLease, (wda_settings::SettingsError, wda_settings::Restoration)>
{
    let guarded = OwnedRequests {
        wda,
        session,
        stopped: Cell::new(false),
    };
    wda_settings::IdleSettingsLease::start(&guarded, session).await
}

pub(crate) async fn restore_owned_idle_settings(
    wda: &impl WdaRequest,
    lease: wda_settings::IdleSettingsLease,
) -> wda_settings::Restoration {
    let session = lease.session().to_owned();
    let guarded = OwnedRequests {
        wda,
        session: &session,
        stopped: Cell::new(false),
    };
    lease.restore(&guarded).await
}

/// Requires a reviewed handoff from every other WDA owner for the whole call.
/// `admit` must check the PIN stop gate for unlock; it runs again immediately
/// before the sole creation POST. Clock qualification needs no PIN or stop gate.
///
/// Only the new receipt's exact session is used. Temporary settings and the
/// action are bound to it. No request is retried, no session is deleted, and a
/// lost create receipt never falls back to discovery. The created session is
/// retained for explicit owner handoff. Await completion to allow restoration.
pub async fn with_owned_session_idle_settings<T, E, F, Fut>(
    wda: &UsbWda,
    admit: impl FnMut() -> Result<(), ()>,
    action: F,
) -> Result<IdleSettingsOutcome<T, E>, OwnedSessionError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    owned_transaction(wda, admit, action).await
}

/// Retain the exact created session ID for one caller-owned close after the
/// settings transaction has finished. A failed creation yields no close target.
pub async fn with_owned_session_idle_settings_receipt<T, E, F, Fut>(
    wda: &UsbWda,
    admit: impl FnMut() -> Result<(), ()>,
    action: F,
) -> Result<(String, IdleSettingsOutcome<T, E>), OwnedSessionError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    owned_transaction_with_receipt(wda, admit, action).await
}

async fn owned_transaction<T, E, F, Fut>(
    wda: &impl WdaRequest,
    admit: impl FnMut() -> Result<(), ()>,
    action: F,
) -> Result<IdleSettingsOutcome<T, E>, OwnedSessionError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    owned_transaction_with_receipt(wda, admit, action)
        .await
        .map(|(_, outcome)| outcome)
}

async fn owned_transaction_with_receipt<T, E, F, Fut>(
    wda: &impl WdaRequest,
    admit: impl FnMut() -> Result<(), ()>,
    action: F,
) -> Result<(String, IdleSettingsOutcome<T, E>), OwnedSessionError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let session = create_session(wda, admit).await?;
    let guarded = OwnedRequests {
        wda,
        session: &session,
        stopped: Cell::new(false),
    };
    let outcome = wda_settings::transaction(&guarded, &session, || action(session.clone())).await;
    Ok((session, outcome))
}

async fn create_session(
    wda: &impl WdaRequest,
    mut admit: impl FnMut() -> Result<(), ()>,
) -> Result<String, OwnedSessionError> {
    admit().map_err(|_| OwnedSessionError::AdmissionRefused)?;
    let status = wda
        .request("GET", "/status", b"", DEADLINE)
        .await
        .map_err(|_| OwnedSessionError::PreflightUnproven)?;
    let status = envelope(&status).map_err(|_| OwnedSessionError::PreflightUnproven)?;
    if status.value["ready"] != true {
        return Err(OwnedSessionError::PreflightUnproven);
    }
    let previous = status
        .id()
        .map_err(|_| OwnedSessionError::PreflightUnproven)?;
    admit().map_err(|_| OwnedSessionError::AdmissionRefused)?;
    let receipt = wda
        .request("POST", "/session", CREATE, DEADLINE)
        .await
        .map_err(|_| OwnedSessionError::CreationUnknown)?;
    let receipt = envelope(&receipt).map_err(|_| OwnedSessionError::CreationUnknown)?;
    let session = receipt
        .id()
        .ok()
        .flatten()
        .filter(|id| Some(*id) != previous)
        .ok_or(OwnedSessionError::CreationUnknown)?;
    let guarded = OwnedRequests {
        wda,
        session,
        stopped: Cell::new(false),
    };
    guarded
        .validate()
        .await
        .map_err(|_| OwnedSessionError::SessionUnproven)?;
    Ok(session.to_owned())
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    error: Option<Value>,
    status: Option<u16>,
    value: Value,
}

impl Envelope {
    fn id(&self) -> Result<Option<&str>, ()> {
        let top = self.session_id.as_deref();
        let nested = match self.value.get("sessionId") {
            None => None,
            Some(value) => Some(value.as_str().ok_or(())?),
        };
        if top.zip(nested).is_some_and(|(top, nested)| top != nested) {
            return Err(());
        }
        let id = top.or(nested);
        if id.is_some_and(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(());
        }
        Ok(id)
    }
}

fn envelope(response: &HttpResponse) -> Result<Envelope, ()> {
    if response.status != 200 {
        return Err(());
    }
    let value: Envelope = serde_json::from_slice(&response.body).map_err(|_| ())?;
    if value.error.is_some()
        || value.status.is_some_and(|status| status != 0)
        || !value.value.is_object()
        || value.value.get("error").is_some()
    {
        return Err(());
    }
    value.id()?;
    Ok(value)
}

struct OwnedRequests<'a, W> {
    wda: &'a W,
    session: &'a str,
    stopped: Cell<bool>,
}

impl<W: WdaRequest> OwnedRequests<'_, W> {
    async fn validate(&self) -> Result<(), WdaError> {
        for path in ["/status".to_owned(), format!("/session/{}", self.session)] {
            let response = self.wda.request("GET", &path, b"", DEADLINE).await?;
            self.check(&path, &response)?;
        }
        Ok(())
    }

    fn check(&self, path: &str, response: &HttpResponse) -> Result<(), WdaError> {
        let session_check = path == "/status" || path == format!("/session/{}", self.session);
        let valid = if session_check {
            envelope(response).is_ok_and(|result| {
                result.id() == Ok(Some(self.session))
                    && (path != "/status"
                        || (result.session_id.as_deref() == Some(self.session)
                            && result.value["ready"] == true))
            })
        } else {
            // Missing/malformed settings receipts keep the bracket's existing
            // restoration path. An explicit conflicting identity stops it.
            match serde_json::from_slice::<Envelope>(&response.body) {
                Ok(result) => result
                    .id()
                    .is_ok_and(|id| id.is_none_or(|id| id == self.session)),
                Err(_) => true,
            }
        };
        if !valid {
            self.stopped.set(true);
            return Err(WdaError::NotConnected);
        }
        Ok(())
    }
}

impl<W: WdaRequest> WdaRequest for OwnedRequests<'_, W> {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError> {
        if self.stopped.get() {
            return Err(WdaError::NotConnected);
        }
        if method == "POST"
            && let Err(error) = self.validate().await
        {
            self.stopped.set(true);
            return Err(error);
        }
        let response = self.wda.request(method, path, body, deadline).await?;
        self.check(path, &response)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wda_settings::{IdleAction, Restoration};
    use serde_json::json;
    use std::cell::RefCell;

    struct Fake {
        requests: RefCell<Vec<(String, String)>>,
        create: RefCell<Option<Result<HttpResponse, WdaError>>>,
        response_override: RefCell<Option<(usize, Result<HttpResponse, WdaError>)>>,
        current: Cell<&'static str>,
        settings: RefCell<Value>,
        drift_at_status: Option<usize>,
        status_reads: Cell<usize>,
    }

    fn response(value: Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    impl Fake {
        fn new(receipt: Result<HttpResponse, WdaError>) -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                create: RefCell::new(Some(receipt)),
                response_override: RefCell::new(None),
                current: Cell::new("prior-session"),
                settings: RefCell::new(
                    json!({"waitForIdleTimeout":10,"animationCoolOffTimeout":2}),
                ),
                drift_at_status: None,
                status_reads: Cell::new(0),
            }
        }

        fn ready() -> Self {
            Self::new(Ok(response(
                json!({"sessionId":"owned-session","value":{"sessionId":"owned-session","capabilities":{}}}),
            )))
        }

        fn posts(&self) -> Vec<String> {
            self.requests
                .borrow()
                .iter()
                .filter(|(method, _)| method == "POST")
                .map(|(_, path)| path.clone())
                .collect()
        }
    }

    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            deadline: Duration,
        ) -> Result<HttpResponse, WdaError> {
            assert_eq!(deadline, DEADLINE);
            self.requests
                .borrow_mut()
                .push((method.into(), path.into()));
            if self
                .response_override
                .borrow()
                .as_ref()
                .is_some_and(|(request, _)| *request == self.requests.borrow().len())
            {
                return self.response_override.borrow_mut().take().unwrap().1;
            }
            match (method, path) {
                ("GET", "/status") => {
                    self.status_reads.set(self.status_reads.get() + 1);
                    if self.drift_at_status == Some(self.status_reads.get()) {
                        self.current.set("replacement");
                    }
                    Ok(response(
                        json!({"sessionId":self.current.get(),"value":{"ready":true}}),
                    ))
                }
                ("POST", "/session") => {
                    assert_eq!(body, CREATE);
                    self.current.set("owned-session");
                    self.create
                        .borrow_mut()
                        .take()
                        .expect("never retry creation")
                }
                ("GET", "/session/owned-session") => {
                    Ok(response(json!({"sessionId":self.current.get(),"value":{}})))
                }
                ("DELETE", "/session/owned-session") => {
                    assert!(body.is_empty());
                    Ok(response(json!({"value":null})))
                }
                ("GET", "/session/owned-session/appium/settings") => Ok(response(
                    json!({"sessionId":self.current.get(),"value":self.settings.borrow().clone()}),
                )),
                ("POST", "/session/owned-session/appium/settings") => {
                    *self.settings.borrow_mut() =
                        serde_json::from_slice::<Value>(body).unwrap()["settings"].clone();
                    Ok(response(
                        json!({"sessionId":self.current.get(),"value":null}),
                    ))
                }
                _ => panic!("unexpected request {method} {path}"),
            }
        }
    }

    #[tokio::test]
    async fn exact_session_close_sends_one_delete_after_validation() {
        let fake = Fake::ready();
        fake.current.set("owned-session");
        assert_eq!(
            close_session(&fake, "owned-session").await,
            SessionClose::Acknowledged
        );
        assert_eq!(
            *fake.requests.borrow(),
            [
                ("GET".into(), "/status".into()),
                ("GET".into(), "/session/owned-session".into()),
                ("DELETE".into(), "/session/owned-session".into()),
            ]
        );
    }

    #[tokio::test]
    async fn drift_or_bad_session_refuses_close_before_delete() {
        for session in ["replacement", "../owned-session", ""] {
            let fake = Fake::ready();
            fake.current.set("owned-session");
            assert_eq!(close_session(&fake, session).await, SessionClose::Refused);
            assert!(
                fake.requests
                    .borrow()
                    .iter()
                    .all(|(method, _)| method != "DELETE")
            );
        }
    }

    #[tokio::test]
    async fn conflicting_session_envelopes_refuse_close_before_delete() {
        for (request, reply) in [
            (1, json!({"value":{"ready":true}})),
            (
                1,
                json!({"sessionId":"owned-session","value":{"ready":true,"sessionId":"replacement"}}),
            ),
            (2, json!({"value":{}})),
            (
                2,
                json!({"sessionId":"owned-session","value":{"sessionId":"replacement"}}),
            ),
            (
                2,
                json!({"sessionId":"owned-session","value":{},"error":"failed"}),
            ),
            (
                2,
                json!({"sessionId":"owned-session","value":{},"status":1}),
            ),
        ] {
            let fake = Fake::ready();
            fake.current.set("owned-session");
            *fake.response_override.borrow_mut() = Some((request, Ok(response(reply))));
            assert_eq!(
                close_session(&fake, "owned-session").await,
                SessionClose::Refused
            );
            assert_eq!(fake.requests.borrow().len(), request);
            assert!(
                fake.requests
                    .borrow()
                    .iter()
                    .all(|(method, _)| method != "DELETE")
            );
        }
    }

    #[tokio::test]
    async fn uncertain_close_receipt_is_never_retried() {
        for reply in [
            Err(WdaError::TimedOutAfterSent),
            Ok(HttpResponse {
                status: 500,
                body: b"failure".to_vec(),
            }),
            Ok(response(json!({"value":{"error":"failed"}}))),
            Ok(response(json!({"value":null,"sessionId":"replacement"}))),
        ] {
            let fake = Fake::ready();
            fake.current.set("owned-session");
            *fake.response_override.borrow_mut() = Some((3, reply));
            assert_eq!(
                close_session(&fake, "owned-session").await,
                SessionClose::Unknown
            );
            assert_eq!(fake.requests.borrow().len(), 3);
            assert_eq!(fake.requests.borrow()[2].0, "DELETE");
        }
    }

    #[tokio::test]
    async fn owned_idle_lease_restores_before_session_close() {
        let fake = Fake::ready();
        fake.current.set("owned-session");
        let lease = start_owned_idle_settings(&fake, "owned-session")
            .await
            .unwrap_or_else(|_| panic!("idle setup failed"));
        assert_eq!(fake.settings.borrow()["waitForIdleTimeout"], 1.0);
        assert_eq!(fake.settings.borrow()["animationCoolOffTimeout"], 0.2);
        assert_eq!(fake.posts(), vec!["/session/owned-session/appium/settings"]);
        assert_eq!(
            restore_owned_idle_settings(&fake, lease).await,
            Restoration::Verified
        );
        assert_eq!(fake.settings.borrow()["waitForIdleTimeout"], 10);
        assert_eq!(
            close_session(&fake, "owned-session").await,
            SessionClose::Acknowledged
        );
        let requests = fake.requests.borrow();
        let writes: Vec<_> = requests
            .iter()
            .filter(|(method, _)| method != "GET")
            .collect();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].0, "POST");
        assert_eq!(writes[1].0, "POST");
        assert_eq!(writes[2].0, "DELETE");
    }

    #[tokio::test]
    async fn owned_idle_lease_refuses_foreign_session_without_restoration_write() {
        let fake = Fake::ready();
        fake.current.set("owned-session");
        let lease = start_owned_idle_settings(&fake, "owned-session")
            .await
            .unwrap_or_else(|_| panic!("idle setup failed"));
        fake.current.set("replacement");
        assert!(matches!(
            restore_owned_idle_settings(&fake, lease).await,
            Restoration::Unproven(_)
        ));
        assert_eq!(fake.posts().len(), 1);
    }

    #[tokio::test]
    async fn standalone_creation_returns_exact_id_without_settings_or_action() {
        for receipt in [
            json!({"sessionId":"owned-session","value":{}}),
            json!({"value":{"sessionId":"owned-session","capabilities":{}}}),
            json!({"sessionId":"owned-session","value":{"sessionId":"owned-session"}}),
        ] {
            let fake = Fake::new(Ok(response(receipt)));
            let checks = Cell::new(0);
            let session = create_session(&fake, || {
                assert_eq!(fake.requests.borrow().len(), checks.get());
                checks.set(checks.get() + 1);
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!(session, "owned-session");
            assert_eq!(checks.get(), 2);
            assert_eq!(
                *fake.requests.borrow(),
                [
                    ("GET".into(), "/status".into()),
                    ("POST".into(), "/session".into()),
                    ("GET".into(), "/status".into()),
                    ("GET".into(), "/session/owned-session".into()),
                ]
            );
            assert_eq!(
                *fake.settings.borrow(),
                json!({"waitForIdleTimeout":10,"animationCoolOffTimeout":2})
            );
        }
    }

    #[tokio::test]
    async fn standalone_preflight_and_validation_failures_stop_at_failed_request() {
        for (request, error) in [
            (1, OwnedSessionError::PreflightUnproven),
            (3, OwnedSessionError::SessionUnproven),
            (4, OwnedSessionError::SessionUnproven),
        ] {
            for reply in [
                Err(WdaError::NotConnected),
                Ok(HttpResponse {
                    status: 500,
                    body: b"unprinted failure".to_vec(),
                }),
                Ok(HttpResponse {
                    status: 200,
                    body: b"malformed".to_vec(),
                }),
                Ok(response(
                    json!({"sessionId":"owned-session","value":{"sessionId":"different"}}),
                )),
                Ok(response(
                    json!({"sessionId":"owned-session","value":{"error":"failed"}}),
                )),
            ] {
                let fake = Fake::ready();
                *fake.response_override.borrow_mut() = Some((request, reply));
                assert_eq!(create_session(&fake, || Ok(())).await, Err(error));
                assert_eq!(fake.requests.borrow().len(), request);
                assert_eq!(
                    fake.posts(),
                    if request == 1 {
                        vec![]
                    } else {
                        vec!["/session"]
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn standalone_unready_or_drifted_session_is_never_adopted() {
        for (request, reply, error) in [
            (
                1,
                json!({"value":{"ready":false}}),
                OwnedSessionError::PreflightUnproven,
            ),
            (
                3,
                json!({"sessionId":"owned-session","value":{"ready":false}}),
                OwnedSessionError::SessionUnproven,
            ),
            (
                3,
                json!({"sessionId":"replacement","value":{"ready":true}}),
                OwnedSessionError::SessionUnproven,
            ),
            (
                4,
                json!({"sessionId":"replacement","value":{}}),
                OwnedSessionError::SessionUnproven,
            ),
        ] {
            let fake = Fake::ready();
            *fake.response_override.borrow_mut() = Some((request, Ok(response(reply))));
            assert_eq!(create_session(&fake, || Ok(())).await, Err(error));
            assert_eq!(fake.requests.borrow().len(), request);
        }
    }

    #[tokio::test]
    async fn new_receipt_brackets_one_action_and_restores_new_session_only() {
        for receipt in [
            json!({"sessionId":"owned-session","value":{}}),
            json!({"value":{"sessionId":"owned-session","capabilities":{}}}),
            json!({"sessionId":"owned-session","value":{"sessionId":"owned-session"}}),
        ] {
            let fake = Fake::new(Ok(response(receipt)));
            let actions = Cell::new(0);
            let outcome = owned_transaction(
                &fake,
                || Ok(()),
                |session| {
                    assert_eq!(session, "owned-session");
                    async {
                        assert_eq!(fake.settings.borrow()["waitForIdleTimeout"], 1.0);
                        actions.set(actions.get() + 1);
                        Ok::<_, ()>(())
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(outcome.action, IdleAction::Completed(Ok(())));
            assert_eq!(outcome.restoration, Restoration::Verified);
            assert_eq!(actions.get(), 1);
            assert_eq!(
                *fake.settings.borrow(),
                json!({"waitForIdleTimeout":10,"animationCoolOffTimeout":2})
            );
            assert_eq!(
                fake.posts(),
                [
                    "/session",
                    "/session/owned-session/appium/settings",
                    "/session/owned-session/appium/settings"
                ]
            );
            assert_eq!(fake.current.get(), "owned-session");
        }
    }

    #[tokio::test]
    async fn transaction_receipt_identifies_the_created_session_after_action_failure() {
        let fake = Fake::ready();
        let (session, outcome) = owned_transaction_with_receipt(
            &fake,
            || Ok(()),
            |session| {
                assert_eq!(session, "owned-session");
                async { Err::<(), _>("action failed") }
            },
        )
        .await
        .unwrap();
        assert_eq!(session, "owned-session");
        assert_eq!(outcome.action, IdleAction::Completed(Err("action failed")));
        assert_eq!(outcome.restoration, Restoration::Verified);
        assert_eq!(fake.posts().len(), 3);
        assert!(
            fake.requests
                .borrow()
                .iter()
                .all(|(method, _)| method != "DELETE")
        );
    }

    #[tokio::test]
    async fn failed_creation_returns_no_session_receipt_or_action() {
        let fake = Fake::new(Err(WdaError::TimedOutAfterSent));
        let action_ran = Cell::new(false);
        let result = owned_transaction_with_receipt(
            &fake,
            || Ok(()),
            |_| async {
                action_ran.set(true);
                Ok::<(), ()>(())
            },
        )
        .await;
        assert!(matches!(result, Err(OwnedSessionError::CreationUnknown)));
        assert!(!action_ran.get());
        assert_eq!(fake.posts(), ["/session"]);
    }

    #[tokio::test]
    async fn ambiguous_create_never_discovers_adopts_retries_or_runs_action() {
        for error in [
            WdaError::AfterSent,
            WdaError::TimedOutAfterSent,
            WdaError::NotConnected,
        ] {
            let fake = Fake::new(Err(error));
            let result = create_session(&fake, || Ok(())).await;
            assert_eq!(result, Err(OwnedSessionError::CreationUnknown));
            assert_eq!(
                *fake.requests.borrow(),
                [
                    ("GET".into(), "/status".into()),
                    ("POST".into(), "/session".into())
                ]
            );
        }
    }

    #[tokio::test]
    async fn conflicting_missing_invalid_and_reused_create_ids_refuse() {
        for receipt in [
            json!({"sessionId":"owned-session","value":{"sessionId":"different"}}),
            json!({"sessionId":"owned-session","value":{"sessionId":42}}),
            json!({"sessionId":"owned-session","value":{"sessionId":null}}),
            json!({"value":{}}),
            json!({"sessionId":"prior-session","value":{}}),
            json!({"sessionId":"bad/id","value":{}}),
            json!({"sessionId":"owned-session","value":{"error":"session not created"}}),
            json!({"sessionId":"owned-session","error":"failed","value":{}}),
            json!({"sessionId":"owned-session","status":13,"value":{}}),
        ] {
            let fake = Fake::new(Ok(response(receipt)));
            let result = create_session(&fake, || Ok(())).await;
            assert_eq!(result, Err(OwnedSessionError::CreationUnknown));
            assert_eq!(fake.requests.borrow().len(), 2);
        }
    }

    #[tokio::test]
    async fn disabled_or_failed_gate_prevents_creation_including_after_preflight() {
        for fail_on in [1, 2] {
            let fake = Fake::ready();
            let checks = Cell::new(0);
            let result = create_session(&fake, || {
                checks.set(checks.get() + 1);
                if checks.get() == fail_on {
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await;
            assert_eq!(result, Err(OwnedSessionError::AdmissionRefused));
            assert!(fake.posts().is_empty());
            assert_eq!(fake.requests.borrow().len(), fail_on - 1);
        }
    }

    #[tokio::test]
    async fn drift_before_settings_or_action_latches_stop_without_restoration_post() {
        // Each validation boundary from creation through the final pre-action read.
        for status_read in 2..=6 {
            let mut fake = Fake::ready();
            fake.drift_at_status = Some(status_read);
            let result = owned_transaction(&fake, || Ok(()), |_| async { Err::<(), ()>(()) }).await;
            if status_read == 2 {
                assert_eq!(result, Err(OwnedSessionError::SessionUnproven));
            } else {
                assert!(matches!(result.unwrap().action, IdleAction::NotRun(_)));
            }
            assert_eq!(fake.status_reads.get(), status_read);
            assert_eq!(fake.posts().len(), if status_read == 6 { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn drift_after_action_refuses_restore_instead_of_touching_replacement() {
        let fake = Fake::ready();
        let result = owned_transaction(
            &fake,
            || Ok(()),
            |_| async {
                fake.current.set("replacement");
                Ok::<_, ()>(())
            },
        )
        .await
        .unwrap();
        assert_eq!(result.action, IdleAction::Completed(Ok(())));
        assert!(matches!(result.restoration, Restoration::Unproven(_)));
        assert_eq!(
            fake.posts(),
            ["/session", "/session/owned-session/appium/settings"]
        );
    }

    #[test]
    fn validation_requires_both_ready_status_and_exact_session_endpoint() {
        let fake = Fake::ready();
        for path in ["/status", "/session/owned-session"] {
            for invalid in [
                json!({"sessionId":"replacement","value":{"ready":true}}),
                json!({"sessionId":"owned-session","value":{"sessionId":"replacement","ready":true}}),
                json!({"value":{"ready":true}}),
                json!({"sessionId":"owned-session","value":{"error":"failed","ready":true}}),
            ] {
                let guard = OwnedRequests {
                    wda: &fake,
                    session: "owned-session",
                    stopped: Cell::new(false),
                };
                assert!(guard.check(path, &response(invalid)).is_err());
                assert!(guard.stopped.get());
            }
        }
    }
}
