//! Explicit session-scoped idle settings observation and restoration.

use crate::wda_action::WdaRequest;
use crate::wda_transport::UsbWda;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::future::Future;
use std::time::{Duration, Instant};

const TEMPORARY: &[u8] =
    br#"{"settings":{"waitForIdleTimeout":1.0,"animationCoolOffTimeout":0.2}}"#;
const DEADLINE: Duration = Duration::from_secs(10);
const RECOVERY_BUDGET: Duration = Duration::from_secs(5);
const KNOWN_ORIGINAL: &str = r#"{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}"#;
const RESTORE_KNOWN: &[u8] =
    br#"{"settings":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnownIdleRecovery {
    Restored,
    AlreadyRestored,
    Refused,
    Unknown,
}

/// One explicit recovery for the known 10/2 preimage. Requires exclusive ownership.
/// Only the stranded temporary pair is writable; no retries or device input.
pub async fn restore_known_idle(wda: &UsbWda) -> KnownIdleRecovery {
    let fresh_until = Instant::now() + RECOVERY_BUDGET;
    recover_known(
        wda,
        &wda.with_write_deadline(fresh_until),
        fresh_until,
        Instant::now,
    )
    .await
}

async fn recover_known(
    reader: &impl WdaRequest,
    writer: &impl WdaRequest,
    fresh_until: Instant,
    now: impl Fn() -> Instant,
) -> KnownIdleRecovery {
    let preflight = async {
        let session = active_session(reader).await?;
        let first = read(reader, &session).await?;
        validate_session(reader, &session).await?;
        let second = read(reader, &session).await?;
        validate_session(reader, &session).await?;
        if !first.same(&second) || now() >= fresh_until {
            return Err(SettingsError::VerificationFailed);
        }
        Ok((session, second))
    }
    .await;
    let Ok((session, current)) = preflight else {
        return KnownIdleRecovery::Refused;
    };
    let original: Settings = serde_json::from_str(KNOWN_ORIGINAL).expect("constant settings");
    if current.same(&original) {
        return KnownIdleRecovery::AlreadyRestored;
    }
    if !current.is_temporary() {
        return KnownIdleRecovery::Refused;
    }
    let receipt = write(writer, &session, RESTORE_KNOWN).await;
    // Both checks are attempted even after an uncertain receipt or failed readback.
    let readback = read(reader, &session).await;
    let same_session = validate_session(reader, &session).await;
    if receipt.is_ok()
        && same_session.is_ok()
        && readback.is_ok_and(|settings| settings.same(&original))
    {
        KnownIdleRecovery::Restored
    } else {
        KnownIdleRecovery::Unknown
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsError {
    SessionUnproven,
    ReadUnproven,
    InvalidSettings,
    ConcurrentChange,
    WriteUnproven,
    VerificationFailed,
}

#[derive(Debug, Eq, PartialEq)]
pub enum IdleAction<T, E> {
    NotRun(SettingsError),
    Completed(Result<T, E>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Restoration {
    NotNeeded,
    Verified,
    Unproven(SettingsError),
}

#[must_use = "the action result and restoration result must both be checked"]
#[derive(Debug, Eq, PartialEq)]
pub struct IdleSettingsOutcome<T, E> {
    pub action: IdleAction<T, E>,
    pub restoration: Restoration,
}

/// Only the two validated numeric settings; no session or other response data.
pub struct IdleTimeouts(Settings);

impl IdleTimeouts {
    pub fn wait_for_idle_timeout(&self) -> &str {
        self.0.idle.get()
    }

    pub fn animation_cool_off_timeout(&self) -> &str {
        self.0.animation.get()
    }
}

/// Read the active existing session's settings without creating or changing it.
pub async fn read_active_idle_settings(wda: &UsbWda) -> Result<IdleTimeouts, SettingsError> {
    read_active(wda).await
}

async fn read_active(wda: &impl WdaRequest) -> Result<IdleTimeouts, SettingsError> {
    let session = active_session(wda).await?;
    let settings = read(wda, &session).await?;
    validate_session(wda, &session).await?;
    Ok(IdleTimeouts(settings))
}

async fn active_session(wda: &impl WdaRequest) -> Result<String, SettingsError> {
    let status = wda
        .request("GET", "/status", b"", DEADLINE)
        .await
        .map_err(|_| SettingsError::SessionUnproven)?;
    if status.status != 200 {
        return Err(SettingsError::SessionUnproven);
    }
    let status: Envelope<Value> =
        serde_json::from_slice(&status.body).map_err(|_| SettingsError::SessionUnproven)?;
    if status.value["ready"].as_bool() != Some(true) || status.value.get("error").is_some() {
        return Err(SettingsError::SessionUnproven);
    }
    let session = status.session_id.ok_or(SettingsError::SessionUnproven)?;
    validate_session(wda, &session).await?;
    Ok(session)
}

/// Run one action only after the temporary settings are acknowledged and verified.
/// Restore the original two numeric values after every attempted set, even when
/// that set or the action fails. No request or action is retried.
///
/// The caller must provide exclusive session ownership and await completion:
/// dropping this future cannot perform asynchronous restoration. Readback checks
/// detect observed conflicts, but WDA offers no atomic compare-and-set or lease.
/// This helper creates no session and is not used by the PIN backend.
pub async fn with_temporary_idle_settings<T, E, F, Fut>(
    wda: &UsbWda,
    session: &str,
    action: F,
) -> IdleSettingsOutcome<T, E>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    transaction(wda, session, action).await
}

async fn transaction<T, E, F, Fut>(
    wda: &impl WdaRequest,
    session: &str,
    action: F,
) -> IdleSettingsOutcome<T, E>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let original = match preimage(wda, session).await {
        Ok(original) => original,
        Err(reason) => {
            return IdleSettingsOutcome {
                action: IdleAction::NotRun(reason),
                restoration: Restoration::NotNeeded,
            };
        }
    };
    let receipt = write(wda, session, TEMPORARY).await;
    let verified = verify_temporary(wda, session).await;
    let action = match receipt.and(verified) {
        Ok(()) => IdleAction::Completed(action().await),
        Err(reason) => IdleAction::NotRun(reason),
    };
    let restoration = match restore(wda, session, &original).await {
        Ok(()) => Restoration::Verified,
        Err(reason) => Restoration::Unproven(reason),
    };
    IdleSettingsOutcome {
        action,
        restoration,
    }
}

async fn preimage(wda: &impl WdaRequest, session: &str) -> Result<Settings, SettingsError> {
    validate_session(wda, session).await?;
    let original = read(wda, session).await?;
    let current = read(wda, session).await?;
    if !current.same(&original) {
        return Err(SettingsError::ConcurrentChange);
    }
    validate_session(wda, session).await?;
    Ok(original)
}

async fn restore(
    wda: &impl WdaRequest,
    session: &str,
    original: &Settings,
) -> Result<(), SettingsError> {
    validate_session(wda, session).await?;
    let current = read(wda, session).await?;
    if !current.is_owned_transition(original) {
        return Err(SettingsError::ConcurrentChange);
    }
    let body = serde_json::to_vec(&SettingsBody { settings: original })
        .map_err(|_| SettingsError::InvalidSettings)?;
    validate_session(wda, session).await?;
    let receipt = write(wda, session, &body).await;
    // Verify even a lost restoration receipt, but retain its uncertainty.
    let verified = verify(wda, session, original).await;
    receipt.and(verified)
}

async fn verify(
    wda: &impl WdaRequest,
    session: &str,
    expected: &Settings,
) -> Result<(), SettingsError> {
    let current = read(wda, session).await?;
    validate_session(wda, session).await?;
    if current.same(expected) {
        Ok(())
    } else {
        Err(SettingsError::VerificationFailed)
    }
}

async fn verify_temporary(wda: &impl WdaRequest, session: &str) -> Result<(), SettingsError> {
    let current = read(wda, session).await?;
    validate_session(wda, session).await?;
    if current.is_temporary() {
        Ok(())
    } else {
        Err(SettingsError::VerificationFailed)
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    value: T,
}

async fn response<T: serde::de::DeserializeOwned>(
    wda: &impl WdaRequest,
    session: &str,
    method: &str,
    path: &str,
    body: &[u8],
    failure: SettingsError,
) -> Result<Envelope<T>, SettingsError> {
    let response = wda
        .request(method, path, body, DEADLINE)
        .await
        .map_err(|_| failure)?;
    if response.status != 200 {
        return Err(failure);
    }
    let result: Envelope<T> = serde_json::from_slice(&response.body).map_err(|_| failure)?;
    if result.session_id.as_deref().is_some_and(|id| id != session) {
        return Err(SettingsError::SessionUnproven);
    }
    Ok(result)
}

async fn validate_session(wda: &impl WdaRequest, session: &str) -> Result<(), SettingsError> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(SettingsError::SessionUnproven);
    }
    let status: Envelope<Value> = response(
        wda,
        session,
        "GET",
        "/status",
        b"",
        SettingsError::SessionUnproven,
    )
    .await?;
    if status.session_id.as_deref() != Some(session)
        || status.value["ready"].as_bool() != Some(true)
        || status.value.get("error").is_some()
    {
        return Err(SettingsError::SessionUnproven);
    }
    let validation: Envelope<Value> = response(
        wda,
        session,
        "GET",
        &format!("/session/{session}"),
        b"",
        SettingsError::SessionUnproven,
    )
    .await?;
    if !validation.value.is_object() || validation.value.get("error").is_some() {
        return Err(SettingsError::SessionUnproven);
    }
    Ok(())
}

async fn read(wda: &impl WdaRequest, session: &str) -> Result<Settings, SettingsError> {
    let result: Envelope<Settings> = response(
        wda,
        session,
        "GET",
        &format!("/session/{session}/appium/settings"),
        b"",
        SettingsError::ReadUnproven,
    )
    .await?;
    result.value.validate()?;
    Ok(result.value)
}

async fn write(wda: &impl WdaRequest, session: &str, body: &[u8]) -> Result<(), SettingsError> {
    let result: Envelope<Value> = response(
        wda,
        session,
        "POST",
        &format!("/session/{session}/appium/settings"),
        body,
        SettingsError::WriteUnproven,
    )
    .await?;
    if result.value.get("error").is_some() {
        return Err(SettingsError::WriteUnproven);
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct Settings {
    #[serde(rename = "waitForIdleTimeout")]
    idle: Box<RawValue>,
    #[serde(rename = "animationCoolOffTimeout")]
    animation: Box<RawValue>,
}

#[derive(Serialize)]
struct SettingsBody<'a> {
    settings: &'a Settings,
}

impl Settings {
    fn temporary() -> Self {
        Self {
            idle: RawValue::from_string("1.0".into()).expect("constant JSON number"),
            animation: RawValue::from_string("0.2".into()).expect("constant JSON number"),
        }
    }

    fn validate(&self) -> Result<(), SettingsError> {
        number(self.idle.get())?;
        number(self.animation.get())?;
        Ok(())
    }

    fn same(&self, other: &Self) -> bool {
        same_number(&self.idle, &other.idle) && same_number(&self.animation, &other.animation)
    }

    fn is_temporary(&self) -> bool {
        same_number(&self.idle, &Self::temporary().idle)
            && temporary_animation(self.animation.get())
    }

    fn is_owned_transition(&self, original: &Self) -> bool {
        let temporary = Self::temporary();
        (same_number(&self.idle, &original.idle) || same_number(&self.idle, &temporary.idle))
            && (same_number(&self.animation, &original.animation)
                || temporary_animation(self.animation.get()))
    }
}

// Only this known temporary value admits WDA's alternate f64 serialization.
// Original settings and all other numbers retain exact decimal comparisons.
fn temporary_animation(raw: &str) -> bool {
    matches!(number(raw), Ok((digits, exponent))
        if (digits == "2" && exponent == -1)
            || (digits == "20000000000000001" && exponent == -17))
}

fn same_number(left: &RawValue, right: &RawValue) -> bool {
    match (number(left.get()), number(right.get())) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

// Preserve the raw numeric token for restoration, and compare exact decimal
// values without rounding distinct JSON numbers through f64.
fn number(raw: &str) -> Result<(String, i32), SettingsError> {
    let invalid = SettingsError::InvalidSettings;
    if raw.len() > 128 {
        return Err(invalid);
    }
    let value: f64 = serde_json::from_str(raw).map_err(|_| invalid)?;
    if !value.is_finite() || value < 0.0 {
        return Err(invalid);
    }
    let negative = raw.starts_with('-');
    let raw = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exponent) = match raw.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().map_err(|_| invalid)?),
        None => (raw, 0),
    };
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, digits)| digits.len());
    let mut digits = mantissa.replace('.', "").trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Ok(("0".into(), 0));
    }
    if negative {
        return Err(invalid);
    }
    let mut exponent = exponent.checked_sub(fraction as i32).ok_or(invalid)?;
    while digits.ends_with('0') {
        digits.pop();
        exponent = exponent.checked_add(1).ok_or(invalid)?;
    }
    Ok((digits, exponent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wda_http::HttpResponse;
    use crate::wda_transport::WdaError;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    const SESSION: &str = "fixture-session";
    const SETTINGS: &str = "/session/fixture-session/appium/settings";
    const ORIGINAL: &str = r#"{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}"#;
    const TARGET: &str = r#"{"waitForIdleTimeout":1.0,"animationCoolOffTimeout":0.2}"#;

    struct Step {
        method: &'static str,
        path: &'static str,
        body: Vec<u8>,
        response: Result<HttpResponse, WdaError>,
    }

    struct Fake {
        steps: RefCell<VecDeque<Step>>,
        posts: Cell<usize>,
    }

    impl Fake {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: RefCell::new(steps.into()),
                posts: Cell::new(0),
            }
        }

        fn done(&self, posts: usize) {
            assert!(self.steps.borrow().is_empty(), "unconsumed requests");
            assert_eq!(self.posts.get(), posts);
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
            let step = self.steps.borrow_mut().pop_front().expect("extra request");
            assert_eq!((method, path), (step.method, step.path));
            assert_eq!(body, step.body);
            assert_eq!(deadline, DEADLINE);
            if method == "POST" {
                self.posts.set(self.posts.get() + 1);
            }
            step.response
        }
    }

    fn get(path: &'static str, body: &str) -> Step {
        Step {
            method: "GET",
            path,
            body: Vec::new(),
            response: Ok(HttpResponse {
                status: 200,
                body: body.as_bytes().to_vec(),
            }),
        }
    }

    fn session() -> Vec<Step> {
        vec![
            get(
                "/status",
                r#"{"sessionId":"fixture-session","value":{"ready":true}}"#,
            ),
            get("/session/fixture-session", r#"{"value":{}}"#),
        ]
    }

    fn changed_session() -> Step {
        get(
            "/status",
            r#"{"sessionId":"replacement","value":{"ready":true}}"#,
        )
    }

    fn settings(values: &str) -> Step {
        get(SETTINGS, &format!(r#"{{"value":{values}}}"#))
    }

    fn post(values: &str) -> Step {
        Step {
            method: "POST",
            path: SETTINGS,
            body: format!(r#"{{"settings":{values}}}"#).into_bytes(),
            response: Ok(HttpResponse {
                status: 200,
                body: br#"{"value":null}"#.to_vec(),
            }),
        }
    }

    fn preflight(original: &str) -> Vec<Step> {
        let mut steps = session();
        steps.extend([settings(original), settings(original)]);
        steps.extend(session());
        steps
    }

    fn configured(original: &str) -> Vec<Step> {
        let mut steps = preflight(original);
        steps.extend([post(TARGET), settings(TARGET)]);
        steps.extend(session());
        steps
    }

    fn restoration(original: &str, current: &str) -> Vec<Step> {
        let mut steps = session();
        steps.push(settings(current));
        steps.extend(session());
        steps.extend([post(original), settings(original)]);
        steps.extend(session());
        steps
    }

    async fn invoke(fake: &Fake, fail_action: bool) -> (IdleSettingsOutcome<u8, ()>, usize) {
        let calls = Cell::new(0);
        let result = transaction(fake, SESSION, || async {
            calls.set(calls.get() + 1);
            if fail_action { Err(()) } else { Ok(7) }
        })
        .await;
        (result, calls.get())
    }

    fn read_only_preflight() -> Vec<Step> {
        let mut steps = vec![session().remove(0)];
        steps.extend(session());
        steps
    }

    const ROUNDTRIP: &str =
        r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":0.20000000000000001}"#;

    #[tokio::test]
    async fn temporary_roundtrip_verifies_and_restores_without_relaxing_originals() {
        let mut steps = preflight(ORIGINAL);
        steps.extend([post(TARGET), settings(ROUNDTRIP)]);
        steps.extend(session());
        steps.extend(restoration(ORIGINAL, ROUNDTRIP));
        let fake = Fake::new(steps);
        let (result, calls) = invoke(&fake, false).await;
        assert_eq!(result.action, IdleAction::Completed(Ok(7)));
        assert_eq!(result.restoration, Restoration::Verified);
        assert_eq!(calls, 1);
        fake.done(2);

        let mut steps = vec![settings(ROUNDTRIP)];
        steps.extend(session());
        let fake = Fake::new(steps);
        assert_eq!(
            verify(&fake, SESSION, &Settings::temporary()).await,
            Err(SettingsError::VerificationFailed)
        );
        fake.done(0);
        for raw in [
            "0.19999999999999999",
            "0.20000000000000002",
            "0.2000000000000000101",
            "2",
        ] {
            assert!(!temporary_animation(raw));
        }
    }

    fn recovery_preflight(first: &str, second: &str) -> Vec<Step> {
        let mut steps = read_only_preflight();
        steps.push(settings(first));
        steps.extend(session());
        steps.push(settings(second));
        steps.extend(session());
        steps
    }

    async fn recover(fake: &Fake) -> KnownIdleRecovery {
        let now = Instant::now();
        recover_known(fake, fake, now + RECOVERY_BUDGET, || now).await
    }

    #[tokio::test]
    async fn known_recovery_posts_exact_body_once_or_leaves_exact_original_alone() {
        for temporary in [TARGET, ROUNDTRIP] {
            let mut steps = recovery_preflight(temporary, temporary);
            steps.extend([post(ORIGINAL), settings(ORIGINAL)]);
            steps.extend(session());
            let fake = Fake::new(steps);
            assert_eq!(recover(&fake).await, KnownIdleRecovery::Restored);
            fake.done(1);
        }
        let fake = Fake::new(recovery_preflight(ORIGINAL, ORIGINAL));
        assert_eq!(recover(&fake).await, KnownIdleRecovery::AlreadyRestored);
        fake.done(0);
    }

    #[tokio::test]
    async fn known_recovery_refuses_foreign_changed_stale_or_invalid_values_without_post() {
        for values in [
            r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":10,"animationCoolOffTimeout":0.2}"#,
            r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":0.20000000000000002}"#,
            r#"{"waitForIdleTimeout":1.00000000000000001,"animationCoolOffTimeout":0.2}"#,
        ] {
            let fake = Fake::new(recovery_preflight(values, values));
            assert_eq!(recover(&fake).await, KnownIdleRecovery::Refused);
            fake.done(0);
        }
        let fake = Fake::new(recovery_preflight(TARGET, ROUNDTRIP));
        assert_eq!(recover(&fake).await, KnownIdleRecovery::Refused);
        fake.done(0);
        for invalid in [
            "{",
            "null",
            r#"{"waitForIdleTimeout":1,"waitForIdleTimeout":1,"animationCoolOffTimeout":0.2}"#,
            r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":"0.2"}"#,
        ] {
            let mut steps = read_only_preflight();
            steps.push(settings(invalid));
            let fake = Fake::new(steps);
            assert_eq!(recover(&fake).await, KnownIdleRecovery::Refused);
            fake.done(0);
        }
        for late in [Duration::ZERO, Duration::from_nanos(1)] {
            let fake = Fake::new(recovery_preflight(TARGET, TARGET));
            let expires = Instant::now();
            assert_eq!(
                recover_known(&fake, &fake, expires, || expires + late).await,
                KnownIdleRecovery::Refused
            );
            fake.done(0);
        }
    }

    #[tokio::test]
    async fn known_recovery_always_checks_readback_and_session_after_uncertain_receipt() {
        for receipt_error in [false, true] {
            for read_error in [false, true] {
                let mut steps = recovery_preflight(ROUNDTRIP, ROUNDTRIP);
                let mut receipt = post(ORIGINAL);
                receipt.response = if receipt_error {
                    Err(WdaError::AfterSent)
                } else {
                    Ok(HttpResponse {
                        status: 503,
                        body: b"fixture rejection".to_vec(),
                    })
                };
                let mut readback = settings(ORIGINAL);
                if read_error {
                    readback.response = Err(WdaError::NotConnected);
                }
                steps.extend([receipt, readback]);
                steps.extend(session());
                let fake = Fake::new(steps);
                assert_eq!(recover(&fake).await, KnownIdleRecovery::Unknown);
                fake.done(1);
            }
        }
        let mut steps = recovery_preflight(TARGET, TARGET);
        steps.extend([post(ORIGINAL), settings(TARGET)]);
        steps.extend(session());
        let fake = Fake::new(steps);
        assert_eq!(recover(&fake).await, KnownIdleRecovery::Unknown);
        fake.done(1);
    }

    #[tokio::test]
    async fn known_recovery_session_drift_refuses_before_post_and_is_unknown_after() {
        let mut before = read_only_preflight();
        before.extend([settings(TARGET), changed_session()]);
        let fake = Fake::new(before);
        assert_eq!(recover(&fake).await, KnownIdleRecovery::Refused);
        fake.done(0);

        let mut after = recovery_preflight(TARGET, TARGET);
        after.extend([post(ORIGINAL), settings(ORIGINAL), changed_session()]);
        let fake = Fake::new(after);
        assert_eq!(recover(&fake).await, KnownIdleRecovery::Unknown);
        fake.done(1);
    }

    #[tokio::test]
    async fn read_only_snapshot_returns_only_two_validated_values_without_posts() {
        let mut steps = read_only_preflight();
        steps.push(settings(r#"{"waitForIdleTimeout":2.50e0,"animationCoolOffTimeout":0.12500000000000000001,"unrelated":"not-for-output"}"#));
        steps.extend(session());
        let fake = Fake::new(steps);
        let result = read_active(&fake).await.unwrap();
        assert_eq!(result.wait_for_idle_timeout(), "2.50e0");
        assert_eq!(
            result.animation_cool_off_timeout(),
            "0.12500000000000000001"
        );
        fake.done(0);
    }

    #[tokio::test]
    async fn read_only_snapshot_refuses_changed_or_wrong_session_without_posts() {
        let mut before = vec![session().remove(0)];
        before.push(changed_session());
        let mut during = read_only_preflight();
        during.push(get(
            SETTINGS,
            &format!(r#"{{"sessionId":"replacement","value":{ORIGINAL}}}"#),
        ));
        let mut after = read_only_preflight();
        after.extend([settings(ORIGINAL), changed_session()]);
        for steps in [before, during, after] {
            let fake = Fake::new(steps);
            assert!(matches!(
                read_active(&fake).await,
                Err(SettingsError::SessionUnproven)
            ));
            fake.done(0);
        }
        for status in [
            r#"{"value":{"ready":true}}"#,
            r#"{"sessionId":"../invalid","value":{"ready":true}}"#,
            r#"{"sessionId":"fixture-session","value":{"ready":false}}"#,
        ] {
            let fake = Fake::new(vec![get("/status", status)]);
            assert!(matches!(
                read_active(&fake).await,
                Err(SettingsError::SessionUnproven)
            ));
            fake.done(0);
        }
    }

    #[tokio::test]
    async fn read_only_snapshot_refuses_malformed_values_without_posts() {
        for values in [
            "null",
            "{}",
            "{",
            r#"{"waitForIdleTimeout":-1,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":1e999,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":"not-for-output","animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":false}"#,
            r#"{"waitForIdleTimeout":1,"waitForIdleTimeout":2,"animationCoolOffTimeout":2}"#,
        ] {
            let mut steps = read_only_preflight();
            steps.push(settings(values));
            let fake = Fake::new(steps);
            assert!(matches!(
                read_active(&fake).await,
                Err(SettingsError::ReadUnproven | SettingsError::InvalidSettings)
            ));
            fake.done(0);
        }
    }

    #[tokio::test]
    async fn read_only_snapshot_refuses_http_and_transport_failures_without_posts() {
        for at_status in [false, true] {
            for http_failure in [false, true] {
                let mut failed = if at_status {
                    session().remove(0)
                } else {
                    settings(ORIGINAL)
                };
                failed.response = if http_failure {
                    Ok(HttpResponse {
                        status: 503,
                        body: b"not-for-output".to_vec(),
                    })
                } else {
                    Err(WdaError::NotConnected)
                };
                let mut steps = if at_status {
                    vec![]
                } else {
                    read_only_preflight()
                };
                steps.push(failed);
                let fake = Fake::new(steps);
                assert!(matches!(
                    read_active(&fake).await,
                    Err(SettingsError::SessionUnproven | SettingsError::ReadUnproven)
                ));
                fake.done(0);
            }
        }
    }

    #[tokio::test]
    async fn success_and_action_failure_restore_exact_nondefault_numeric_tokens() {
        for original in [
            ORIGINAL,
            r#"{"waitForIdleTimeout":2.7500e0,"animationCoolOffTimeout":0.12500000000000000001}"#,
        ] {
            for fail_action in [false, true] {
                let mut steps = configured(original);
                steps.extend(restoration(original, TARGET));
                let fake = Fake::new(steps);
                let (result, calls) = invoke(&fake, fail_action).await;
                assert_eq!(
                    result,
                    IdleSettingsOutcome {
                        action: IdleAction::Completed(if fail_action { Err(()) } else { Ok(7) }),
                        restoration: Restoration::Verified,
                    }
                );
                assert_eq!(calls, 1);
                fake.done(2);
            }
        }
    }

    #[tokio::test]
    async fn uncertain_or_rejected_set_never_runs_action_and_restores_once() {
        for transport_error in [false, true] {
            for read_error in [false, true] {
                let mut steps = preflight(ORIGINAL);
                let mut set = post(TARGET);
                set.response = if transport_error {
                    Err(WdaError::AfterSent)
                } else {
                    Ok(HttpResponse {
                        status: 200,
                        body: br#"{"value":{"error":"fixture rejection"}}"#.to_vec(),
                    })
                };
                let mut readback = settings(TARGET);
                if read_error {
                    readback.response = Err(WdaError::NotConnected);
                }
                steps.extend([set, readback]);
                if !read_error {
                    steps.extend(session());
                }
                steps.extend(restoration(ORIGINAL, TARGET));
                let fake = Fake::new(steps);
                let (result, calls) = invoke(&fake, false).await;
                assert_eq!(
                    result.action,
                    IdleAction::NotRun(SettingsError::WriteUnproven)
                );
                assert_eq!(result.restoration, Restoration::Verified);
                assert_eq!(calls, 0);
                fake.done(2);
            }
        }
    }

    #[tokio::test]
    async fn unchanged_or_partial_set_is_not_verified_and_is_restored() {
        for current in [
            ORIGINAL,
            r#"{"waitForIdleTimeout":1,"animationCoolOffTimeout":2}"#,
        ] {
            let mut steps = preflight(ORIGINAL);
            steps.extend([post(TARGET), settings(current)]);
            steps.extend(session());
            steps.extend(restoration(ORIGINAL, current));
            let fake = Fake::new(steps);
            let (result, calls) = invoke(&fake, false).await;
            assert_eq!(
                result.action,
                IdleAction::NotRun(SettingsError::VerificationFailed)
            );
            assert_eq!(result.restoration, Restoration::Verified);
            assert_eq!(calls, 0);
            fake.done(2);
        }
    }

    #[tokio::test]
    async fn restoration_failure_is_separate_and_never_replays_action_or_restore() {
        for failure in [
            SettingsError::WriteUnproven,
            SettingsError::ReadUnproven,
            SettingsError::VerificationFailed,
            SettingsError::SessionUnproven,
        ] {
            let mut steps = configured(ORIGINAL);
            steps.extend(session());
            steps.push(settings(TARGET));
            steps.extend(session());
            let mut restore = post(ORIGINAL);
            if failure == SettingsError::WriteUnproven {
                restore.response = Err(WdaError::AfterSent);
            }
            steps.push(restore);
            let mut readback = settings(if failure == SettingsError::VerificationFailed {
                TARGET
            } else {
                ORIGINAL
            });
            if failure == SettingsError::ReadUnproven {
                readback.response = Err(WdaError::NotConnected);
            }
            steps.push(readback);
            if failure == SettingsError::SessionUnproven {
                steps.push(changed_session());
            } else if failure != SettingsError::ReadUnproven {
                steps.extend(session());
            }
            let fake = Fake::new(steps);
            let (result, calls) = invoke(&fake, false).await;
            assert_eq!(result.action, IdleAction::Completed(Ok(7)));
            assert_eq!(result.restoration, Restoration::Unproven(failure));
            assert_eq!(calls, 1);
            fake.done(2);
        }
    }

    #[tokio::test]
    async fn changed_session_before_either_mutation_fails_closed() {
        let mut steps = session();
        steps.extend([settings(ORIGINAL), settings(ORIGINAL), changed_session()]);
        let fake = Fake::new(steps);
        let (result, calls) = invoke(&fake, false).await;
        assert_eq!(
            result.action,
            IdleAction::NotRun(SettingsError::SessionUnproven)
        );
        assert_eq!(result.restoration, Restoration::NotNeeded);
        assert_eq!(calls, 0);
        fake.done(0);

        for change_after_restore_read in [false, true] {
            let mut steps = configured(ORIGINAL);
            if change_after_restore_read {
                steps.extend(session());
                steps.push(settings(TARGET));
            }
            steps.push(changed_session());
            let fake = Fake::new(steps);
            let (result, calls) = invoke(&fake, false).await;
            assert_eq!(result.action, IdleAction::Completed(Ok(7)));
            assert_eq!(
                result.restoration,
                Restoration::Unproven(SettingsError::SessionUnproven)
            );
            assert_eq!(calls, 1);
            fake.done(1);
        }
    }

    #[tokio::test]
    async fn session_change_during_set_verification_blocks_action_and_foreign_restore() {
        let mut steps = preflight(ORIGINAL);
        steps.extend([
            post(TARGET),
            settings(TARGET),
            changed_session(),
            changed_session(),
        ]);
        let fake = Fake::new(steps);
        let (result, calls) = invoke(&fake, false).await;
        assert_eq!(
            result.action,
            IdleAction::NotRun(SettingsError::SessionUnproven)
        );
        assert_eq!(
            result.restoration,
            Restoration::Unproven(SettingsError::SessionUnproven)
        );
        assert_eq!(calls, 0);
        fake.done(1);
    }

    #[tokio::test]
    async fn observed_concurrent_settings_are_never_overwritten() {
        let foreign = r#"{"waitForIdleTimeout":3,"animationCoolOffTimeout":4}"#;
        let mut steps = session();
        steps.extend([settings(ORIGINAL), settings(foreign)]);
        let fake = Fake::new(steps);
        let (result, calls) = invoke(&fake, false).await;
        assert_eq!(
            result.action,
            IdleAction::NotRun(SettingsError::ConcurrentChange)
        );
        assert_eq!(result.restoration, Restoration::NotNeeded);
        assert_eq!(calls, 0);
        fake.done(0);

        let mut steps = configured(ORIGINAL);
        steps.extend(session());
        steps.push(settings(foreign));
        let fake = Fake::new(steps);
        let (result, calls) = invoke(&fake, false).await;
        assert_eq!(result.action, IdleAction::Completed(Ok(7)));
        assert_eq!(
            result.restoration,
            Restoration::Unproven(SettingsError::ConcurrentChange)
        );
        assert_eq!(calls, 1);
        fake.done(1);
    }

    #[tokio::test]
    async fn malformed_missing_duplicate_or_invalid_preimage_sends_no_post() {
        for values in [
            "{}",
            "null",
            r#"{"waitForIdleTimeout":1,"waitForIdleTimeout":2,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":-1,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":1e999,"animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":"10","animationCoolOffTimeout":2}"#,
            r#"{"waitForIdleTimeout":10,"animationCoolOffTimeout":false}"#,
            r#"{"waitForIdleTimeout":10,"animationCoolOffTimeout":null}"#,
        ] {
            let mut steps = session();
            steps.push(settings(values));
            let fake = Fake::new(steps);
            let (result, calls) = invoke(&fake, false).await;
            assert!(matches!(result.action, IdleAction::NotRun(_)));
            assert_eq!(result.restoration, Restoration::NotNeeded);
            assert_eq!(calls, 0);
            fake.done(0);
        }
    }

    #[tokio::test]
    async fn invalid_or_unready_session_and_wrong_readback_session_are_refused() {
        for id in ["", "../invalid"] {
            let fake = Fake::new(vec![]);
            let result = transaction(&fake, id, || async { Ok::<_, ()>(()) }).await;
            assert_eq!(
                result.action,
                IdleAction::NotRun(SettingsError::SessionUnproven)
            );
            fake.done(0);
        }
        for status in [
            r#"{"value":{"ready":true}}"#,
            r#"{"sessionId":"fixture-session","value":{"ready":false}}"#,
            r#"{"sessionId":"replacement","value":{"ready":true}}"#,
        ] {
            let fake = Fake::new(vec![get("/status", status)]);
            assert_eq!(
                invoke(&fake, false).await.0.action,
                IdleAction::NotRun(SettingsError::SessionUnproven)
            );
            fake.done(0);
        }
        let mut steps = session();
        steps.push(get(
            SETTINGS,
            &format!(r#"{{"sessionId":"replacement","value":{ORIGINAL}}}"#),
        ));
        let fake = Fake::new(steps);
        assert_eq!(
            invoke(&fake, false).await.0.action,
            IdleAction::NotRun(SettingsError::SessionUnproven)
        );
        fake.done(0);
    }

    #[test]
    fn decimal_comparison_accepts_normalization_without_rounding_or_negative_underflow() {
        for (left, right) in [
            ("1", "1.0"),
            ("0.20", "2e-1"),
            ("-0", "0.0"),
            ("10e1", "100"),
        ] {
            assert_eq!(number(left), number(right));
        }
        assert_ne!(
            number("0.12500000000000000001"),
            number("0.12500000000000000002")
        );
        assert_ne!(number("9007199254740992"), number("9007199254740993"));
        for invalid in [
            "-1e-999",
            "-0.1",
            "1e999",
            "NaN",
            "null",
            "true",
            "\"1\"",
            "01",
            "1e2147483648",
        ] {
            assert!(number(invalid).is_err());
        }
    }
}
