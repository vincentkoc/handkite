use handkite_core::passcode::PasscodeProgress;
use handkite_core::protocol::{HardwareId, LockState};
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_http::HttpResponse;
use handkite_host::wda_settings::validated_active_session;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{host_config, native_lock, pin_screen};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

const SOURCE: &str = "source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";
const QUERY: &[u8] = br#"{"using":"class name","value":"XCUIElementTypeSecureTextField"}"#;
const MAX_ELAPSED_MS: u128 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Refusal {
    Identity,
    Lock,
    Session,
    Screen,
    Response,
    Element,
    Progress,
    Timing,
}

impl Refusal {
    fn message(self) -> &'static str {
        match self {
            Self::Identity => "refused reason=identity_unproven",
            Self::Lock => "refused reason=lock_unproven",
            Self::Session => "refused reason=session_unproven",
            Self::Screen => "refused reason=screen_unproven",
            Self::Response => "refused reason=response_unproven",
            Self::Element => "refused reason=element_unproven",
            Self::Progress => "refused reason=progress_unproven",
            Self::Timing => "refused reason=timing_unproven",
        }
    }
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("refused reason=invalid_arguments");
    };
    if flag != "--config" {
        return Err("refused reason=invalid_arguments");
    }
    let expected = host_config::load(Path::new(path))
        .map_err(|_| "refused reason=configuration_invalid")?
        .usb_id;
    let mut live = Live::new(expected).map_err(Refusal::message)?;
    let result = inspect(&mut live).map_err(Refusal::message)?;
    println!("{}", result.line());
    Ok(())
}

// POST is used only for the read-only element lookup; there is no input method.
trait ReadDevice {
    fn guard(&mut self) -> Result<String, Refusal>;
    fn request(&mut self, method: &str, path: &str, body: &[u8]) -> Result<HttpResponse, Refusal>;
    fn now(&self) -> Instant;
}

#[derive(Debug, PartialEq)]
struct Comparison {
    direct: u32,
    elapsed_ms: u128,
}
impl Comparison {
    fn line(&self) -> String {
        format!(
            "pin_field_compare=observed source_count=0 direct_count={} total=6 elapsed_ms={}",
            self.direct, self.elapsed_ms
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    value: T,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    status: Option<u16>,
}

fn value<T: serde::de::DeserializeOwned>(
    response: HttpResponse,
    session: &str,
) -> Result<T, Refusal> {
    if response.status != 200 || response.body.len() > 4 * 1024 * 1024 {
        return Err(Refusal::Response);
    }
    let parsed: Envelope<T> =
        serde_json::from_slice(&response.body).map_err(|_| Refusal::Response)?;
    if parsed.status.is_some_and(|status| status != 0) {
        return Err(Refusal::Response);
    }
    if parsed.session.as_deref().is_some_and(|id| id != session) {
        return Err(Refusal::Session);
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

fn single_element(response: HttpResponse, session: &str) -> Result<String, Refusal> {
    let mut elements: Vec<Element> = value(response, session)?;
    if elements.len() != 1 {
        return Err(Refusal::Element);
    }
    let element = elements.pop().ok_or(Refusal::Element)?;
    let id = match (element.legacy, element.w3c) {
        (Some(left), Some(right)) if left == right => left,
        (Some(id), None) | (None, Some(id)) => id,
        _ => return Err(Refusal::Element),
    };
    if !valid_id(&id) {
        return Err(Refusal::Element);
    }
    Ok(id)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn source_empty(response: HttpResponse, session: &str) -> Result<(), Refusal> {
    let xml: String = value(response, session)?;
    if !super::lock_ui_attrs::empty_keypad(&xml) {
        return Err(Refusal::Screen);
    }
    let view = pin_screen::parse(&xml).map_err(|_| Refusal::Screen)?;
    if count(
        view.secure_field_value
            .as_deref()
            .ok_or(Refusal::Progress)?,
    )? != 0
    {
        return Err(Refusal::Progress);
    }
    Ok(())
}

fn count(raw: &str) -> Result<u32, Refusal> {
    if raw.len() > 256 {
        return Err(Refusal::Progress);
    }
    PasscodeProgress::parse(raw, 6)
        .map(|progress| progress.entered)
        .map_err(|_| Refusal::Progress)
}

fn inspect(device: &mut impl ReadDevice) -> Result<Comparison, Refusal> {
    let started = device.now();
    let session = device.guard()?;
    if !valid_id(&session) {
        return Err(Refusal::Session);
    }
    let source_path = format!("/session/{session}/{SOURCE}");
    source_empty(device.request("GET", &source_path, b"")?, &session)?;
    let element = single_element(
        device.request("POST", &format!("/session/{session}/elements"), QUERY)?,
        &session,
    )?;
    let raw: String = value(
        device.request(
            "GET",
            &format!("/session/{session}/element/{element}/attribute/value"),
            b"",
        )?,
        &session,
    )?;
    let direct = count(&raw)?;
    source_empty(device.request("GET", &source_path, b"")?, &session)?;
    if device.guard()? != session {
        return Err(Refusal::Session);
    }
    let elapsed_ms = device
        .now()
        .checked_duration_since(started)
        .ok_or(Refusal::Timing)?
        .as_millis();
    if elapsed_ms > MAX_ELAPSED_MS {
        return Err(Refusal::Timing);
    }
    Ok(Comparison { direct, elapsed_ms })
}

struct Live {
    expected: HardwareId,
    usb: PinnedUsb,
    wda: UsbWda,
    runtime: tokio::runtime::Runtime,
}
impl Live {
    fn new(expected: HardwareId) -> Result<Self, Refusal> {
        let usb = PinnedUsb::new(expected.clone());
        Ok(Self {
            expected,
            wda: UsbWda::new(usb.clone()),
            usb,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| Refusal::Response)?,
        })
    }
}
impl ReadDevice for Live {
    fn guard(&mut self) -> Result<String, Refusal> {
        self.runtime
            .block_on(self.usb.provider())
            .map_err(|_| Refusal::Identity)?;
        let native = native_lock::query(&self.expected);
        if native.state != LockState::Locked || native.unlocked_since_boot != Some(true) {
            return Err(Refusal::Lock);
        }
        let session = self
            .runtime
            .block_on(validated_active_session(&self.wda))
            .map_err(|_| Refusal::Session)?;
        let locked: bool = value(self.request("GET", "/wda/locked", b"")?, &session)?;
        if !locked {
            return Err(Refusal::Lock);
        }
        #[derive(Deserialize)]
        struct ActiveApp {
            #[serde(rename = "bundleId")]
            bundle: String,
        }
        let app: ActiveApp = value(self.request("GET", "/wda/activeAppInfo", b"")?, &session)?;
        if app.bundle != "com.apple.springboard" {
            return Err(Refusal::Screen);
        }
        Ok(session)
    }
    fn request(&mut self, method: &str, path: &str, body: &[u8]) -> Result<HttpResponse, Refusal> {
        self.runtime
            .block_on(
                self.wda
                    .request(method, path, body, Duration::from_secs(10)),
            )
            .map_err(|_| Refusal::Response)
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    const SESSION: &str = "fixture-session";
    fn response(body: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }
    fn source() -> HttpResponse {
        let mut xml = String::from(
            r#"<AppiumAUT><XCUIElementTypeApplication name="SpringBoard" width="400" height="800"><XCUIElementTypeOther name="passcode-view"/>"#,
        );
        for digit in 0..10 {
            let (col, row) = if digit == 0 {
                (1, 3)
            } else {
                ((digit - 1) % 3, (digit - 1) / 3)
            };
            xml.push_str(&format!(r#"<XCUIElementTypeKey name="{digit}" visible="false" enabled="true" x="{}" y="{}" width="80" height="80"/>"#, 40 + col * 100, 300 + row * 100));
        }
        xml.push_str(r#"<XCUIElementTypeSecureTextField visible="false" enabled="true" value="0 of 6 digits" x="0" y="220" width="400" height="13"/><XCUIElementTypeStaticText label="private-canary"/></XCUIElementTypeApplication></AppiumAUT>"#);
        response(json!({"value":xml}))
    }
    struct Step {
        method: &'static str,
        path: String,
        response: Result<HttpResponse, Refusal>,
    }
    struct Fake {
        steps: VecDeque<Step>,
        sessions: VecDeque<Result<String, Refusal>>,
        posts: usize,
        calls: usize,
        started: Instant,
        elapsed: Duration,
    }
    impl Fake {
        fn new(direct: &str) -> Self {
            let source_path = format!("/session/{SESSION}/{SOURCE}");
            Self { steps: [
                Step { method:"GET", path:source_path.clone(), response:Ok(source()) },
                Step { method:"POST", path:format!("/session/{SESSION}/elements"), response:Ok(response(json!({"value":[{"ELEMENT":"fixture-element","element-6066-11e4-a52e-4f735466cecf":"fixture-element"}]}))) },
                Step { method:"GET", path:format!("/session/{SESSION}/element/fixture-element/attribute/value"), response:Ok(response(json!({"value":direct}))) },
                Step { method:"GET", path:source_path, response:Ok(source()) },
            ].into(), sessions:[Ok(SESSION.into()),Ok(SESSION.into())].into(), posts:0, calls:0, started:Instant::now(), elapsed:Duration::from_millis(500) }
        }
    }
    impl ReadDevice for Fake {
        fn guard(&mut self) -> Result<String, Refusal> {
            self.sessions.pop_front().expect("no repeated guard")
        }
        fn request(
            &mut self,
            method: &str,
            path: &str,
            body: &[u8],
        ) -> Result<HttpResponse, Refusal> {
            let step = self.steps.pop_front().expect("no retry or extra request");
            assert_eq!((method, path), (step.method, step.path.as_str()));
            if method == "POST" {
                assert_eq!(body, QUERY);
                self.posts += 1;
            } else {
                assert!(body.is_empty());
            }
            self.calls += 1;
            step.response
        }
        fn now(&self) -> Instant {
            if self.calls == 0 {
                self.started
            } else {
                self.started + self.elapsed
            }
        }
    }
    #[test]
    fn compares_only_counts_with_one_lookup_and_attribute_get() {
        for entered in 0..=6 {
            let mut fake = Fake::new(&format!("{entered} of 6 digits"));
            let result = inspect(&mut fake).unwrap();
            assert_eq!(
                result.line(),
                format!(
                    "pin_field_compare=observed source_count=0 direct_count={entered} total=6 elapsed_ms=500"
                )
            );
            for private in [
                SESSION,
                "fixture-element",
                "private-canary",
                "digits",
                "<",
                "value",
            ] {
                assert!(!result.line().contains(private));
            }
            assert_eq!((fake.posts, fake.calls), (1, 4));
            assert!(fake.steps.is_empty() && fake.sessions.is_empty());
        }
    }
    #[test]
    fn malformed_duplicate_or_ambiguous_elements_never_read_a_value() {
        for body in [
            r#"{"value":[]}"#,
            r#"{"value":[{},{}]}"#,
            r#"{"value":[{}]}"#,
            r#"{"value":[{"ELEMENT":"first","ELEMENT":"second"}]}"#,
            r#"{"value":[{"ELEMENT":"first","element-6066-11e4-a52e-4f735466cecf":"second"}]}"#,
            r#"{"value":[{"ELEMENT":"../unsafe"}]}"#,
            r#"{"value":[],"value":[]}"#,
            r#"{"value":{"error":"private-canary"}}"#,
            r#"{"value":null}"#,
        ] {
            let mut fake = Fake::new("0 of 6 digits");
            fake.steps[1].response = Ok(HttpResponse {
                status: 200,
                body: body.as_bytes().to_vec(),
            });
            assert!(inspect(&mut fake).is_err());
            assert_eq!((fake.posts, fake.calls), (1, 2));
        }
    }
    #[test]
    fn malformed_attribute_or_error_refuses_without_echo_or_retry() {
        for body in [
            r#"{"value":"private-canary"}"#,
            r#"{"value":"0 of 4 digits"}"#,
            r#"{"value":"7 of 6 digits"}"#,
            r#"{"value":"0 of 6 digits","value":"1 of 6 digits"}"#,
            r#"{"value":{"error":"private-canary"}}"#,
            r#"{"value":0}"#,
            r#"{"value":"0 of 6 digits","sessionId":"different-session"}"#,
        ] {
            let mut fake = Fake::new("0 of 6 digits");
            fake.steps[2].response = Ok(HttpResponse {
                status: 200,
                body: body.as_bytes().to_vec(),
            });
            let error = inspect(&mut fake).unwrap_err();
            assert!(!error.message().contains("private-canary"));
            assert_eq!((fake.posts, fake.calls), (1, 3));
        }
        for failed in 0..4 {
            for transport in [false, true] {
                let mut fake = Fake::new("0 of 6 digits");
                fake.steps[failed].response = if transport {
                    Err(Refusal::Response)
                } else {
                    Ok(HttpResponse {
                        status: 500,
                        body: br#"{"value":"private-canary"}"#.to_vec(),
                    })
                };
                assert_eq!(inspect(&mut fake), Err(Refusal::Response));
                assert_eq!(fake.calls, failed + 1);
                assert!(fake.posts <= 1);
            }
        }
    }
    #[test]
    fn source_guard_session_drift_and_timing_fail_closed() {
        for n in 0..2 {
            for reason in [
                Refusal::Identity,
                Refusal::Lock,
                Refusal::Session,
                Refusal::Screen,
            ] {
                let mut fake = Fake::new("0 of 6 digits");
                fake.sessions[n] = Err(reason);
                assert_eq!(inspect(&mut fake), Err(reason));
                assert_eq!(fake.calls, if n == 0 { 0 } else { 4 });
            }
            let mut fake = Fake::new("0 of 6 digits");
            fake.steps[if n == 0 { 0 } else { 3 }].response =
                Ok(response(json!({"value":"<private-canary>"})));
            assert_eq!(inspect(&mut fake), Err(Refusal::Screen));
        }
        let mut fake = Fake::new("0 of 6 digits");
        fake.sessions[1] = Ok("changed-session".into());
        assert_eq!(inspect(&mut fake), Err(Refusal::Session));
        let mut fake = Fake::new("0 of 6 digits");
        fake.elapsed = Duration::from_millis(60_001);
        assert_eq!(inspect(&mut fake), Err(Refusal::Timing));
        assert_eq!(command(&[]), Err("refused reason=invalid_arguments"));
    }
}
