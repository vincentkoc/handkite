use super::*;
use crate::{local_client, local_socket};
use handkite_core::controller::{AdapterError, Controller, DeviceAdapter, RawObservation};
use handkite_core::protocol::{
    DeviceBinding, DeviceState, HardwareId, ImageFormat, LockState, ObservationWindow, ScreenImage,
    TransportReceipt,
};
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct FakeState {
    app: Option<String>,
    actions: Vec<&'static str>,
    fail_action: bool,
    fail_capture_after_action: bool,
    mismatch_after_action: bool,
    oversized_after_action: bool,
}

struct FakeAdapter(Arc<Mutex<FakeState>>);

impl DeviceAdapter for FakeAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        let id = HardwareId::new("fixture-usb").unwrap();
        let state = self.0.lock().unwrap();
        let wda = if state.mismatch_after_action && !state.actions.is_empty() {
            HardwareId::new("fixture-other-usb").unwrap()
        } else {
            id.clone()
        };
        Ok((id, wda))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        let (connected_usb, wda_target) = self.identities()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let state = self.0.lock().unwrap();
        if state.fail_capture_after_action && !state.actions.is_empty() {
            return Err(AdapterError);
        }
        Ok(RawObservation {
            connected_usb,
            wda_target,
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: state.app.clone(),
            },
            accessibility_tree: if state.oversized_after_action && !state.actions.is_empty() {
                "x".repeat(MAX_RESPONSE)
            } else {
                "<private-screen/>".into()
            },
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: b"private-screen".to_vec(),
            },
        })
    }

    fn execute(
        &mut self,
        action: &Action,
        send_deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        if Instant::now() >= send_deadline {
            return Err(AdapterError);
        }
        let mut state = self.0.lock().unwrap();
        state.actions.push(action.kind());
        state.app = Some(match action {
            Action::Home => "com.apple.springboard".into(),
            Action::LaunchApp { bundle_id } => {
                assert_eq!(bundle_id, "com.apple.mobiletimer");
                bundle_id.clone()
            }
            _ => panic!("unauthorized action reached the adapter"),
        });
        if state.fail_action {
            return Err(AdapterError);
        }
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: action.kind().into(),
        })
    }
}

struct SocketFixture {
    directory: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
    state: Arc<Mutex<FakeState>>,
    requests: Vec<Value>,
    tokens: Vec<String>,
    short_ttl: bool,
    lose_reply: Option<&'static str>,
}

impl SocketFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = local_socket::bind_protected(directory.path()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(FakeState::default()));
        let controller = Controller::new(
            DeviceBinding {
                key: device(),
                usb_id: HardwareId::new("fixture-usb").unwrap(),
            },
            FakeAdapter(state.clone()),
            1000,
            5000,
        );
        let worker_stop = stop.clone();
        let server = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        local_socket::handle(&mut stream, &controller).unwrap();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        Self {
            directory,
            stop,
            server: Some(server),
            state,
            requests: vec![],
            tokens: vec![],
            short_ttl: false,
            lose_reply: None,
        }
    }

    fn exchange(&mut self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        let mut request: Value = serde_json::from_slice(bytes).unwrap();
        self.requests.push(request.clone());
        if request["method"] == "acquire" {
            assert_eq!(request["ttl_ms"], session::CONTROL_TTL_MS);
            if self.short_ttl {
                request["ttl_ms"] = json!(10);
            }
        }
        let reply = local_client::exchange(
            &self.directory.path().join("handkite.sock"),
            &serde_json::to_vec(&request).unwrap(),
        )?;
        let parsed: Value = serde_json::from_slice(&reply).unwrap();
        if let Some(token) = parsed["lease"].as_str() {
            self.tokens.push(token.into());
        }
        if request["method"].as_str() == self.lose_reply {
            self.lose_reply = None;
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fixture lost reply after delivery",
            ));
        }
        Ok(reply)
    }

    fn call(&mut self, session: &mut Session, name: &str, args: Value) -> Value {
        process(
            &call(name, args),
            &device(),
            Capabilities::ObservationAndNavigation,
            session,
            &mut |bytes| self.exchange(bytes),
        )
        .unwrap()
    }

    fn transcript(
        &mut self,
        input: &[u8],
        capabilities: Capabilities,
    ) -> (io::Result<()>, Vec<u8>) {
        let mut output = Vec::new();
        let result = serve_with_capabilities(
            Cursor::new(input),
            &mut output,
            &device(),
            capabilities,
            |bytes| self.exchange(bytes),
        );
        (result, output)
    }
}

impl Drop for SocketFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.take().unwrap().join().unwrap();
    }
}

fn device() -> DeviceKey {
    DeviceKey::new("fixture-phone").unwrap()
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn frames(requests: &[Value]) -> Vec<u8> {
    requests
        .iter()
        .flat_map(|request| {
            let mut frame = serde_json::to_vec(request).unwrap();
            frame.push(b'\n');
            frame
        })
        .collect()
}

fn payload(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn capability_catalog_and_dispatch_are_separate_guards() {
    let observation = ["handkite_status", "handkite_observe"];
    let navigation = [
        "handkite_acquire_control",
        "handkite_release_control",
        "handkite_home",
        "handkite_launch_clock",
    ];
    let interaction = [
        "handkite_tap",
        "handkite_click_button",
        "handkite_double_tap",
        "handkite_long_press",
        "handkite_swipe",
        "handkite_text",
        "handkite_key",
        "handkite_launch_app",
    ];
    for (capabilities, expected) in [
        (Capabilities::Observation, observation.to_vec()),
        (Capabilities::Navigation, navigation.to_vec()),
        (
            Capabilities::ObservationAndNavigation,
            [observation.as_slice(), navigation.as_slice()].concat(),
        ),
        (
            Capabilities::ObservationNavigationInteraction,
            [
                observation.as_slice(),
                navigation.as_slice(),
                interaction.as_slice(),
            ]
            .concat(),
        ),
    ] {
        let mut no_exchange = |_: &[u8]| panic!("discovery or unauthorized call reached socket");
        let mut session = Session::default();
        let catalog = process(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            &device(),
            capabilities,
            &mut session,
            &mut no_exchange,
        )
        .unwrap();
        let names: Vec<_> = catalog["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected);
        for name in observation
            .iter()
            .chain(navigation.iter())
            .chain(interaction.iter())
            .chain(
                [
                    "handkite_act",
                    "handkite_tap",
                    "handkite_swipe",
                    "handkite_text",
                    "handkite_lock",
                    "handkite_unlock",
                ]
                .iter(),
            )
        {
            if !expected.contains(name) {
                assert_eq!(
                    process(
                        &call(name, json!({})),
                        &device(),
                        capabilities,
                        &mut session,
                        &mut no_exchange
                    )
                    .unwrap()["error"]["code"],
                    -32602
                );
            }
        }
    }
}

#[test]
fn interaction_requires_explicit_profile_and_control_without_socket_input() {
    for (name, arguments) in [
        (
            "handkite_tap",
            json!({"expected_revision":1,"at":{"x":20,"y":30}}),
        ),
        (
            "handkite_double_tap",
            json!({"expected_revision":1,"at":{"x":20,"y":30}}),
        ),
        (
            "handkite_long_press",
            json!({"expected_revision":1,"at":{"x":20,"y":30},"duration_ms":750}),
        ),
        (
            "handkite_key",
            json!({"expected_revision":1,"key":"backspace"}),
        ),
    ] {
        let action = call(name, arguments);
        for capability in [
            Capabilities::Observation,
            Capabilities::Navigation,
            Capabilities::ObservationAndNavigation,
        ] {
            let reply = process(
                &action,
                &device(),
                capability,
                &mut Session::default(),
                &mut |_| panic!("socket reached"),
            );
            assert_eq!(reply.unwrap()["error"]["code"], -32602);
        }
        let reply = process(
            &action,
            &device(),
            Capabilities::ObservationNavigationInteraction,
            &mut Session::default(),
            &mut |_| panic!("socket reached"),
        );
        assert_eq!(reply.unwrap()["result"]["isError"], true);
    }
}

#[test]
fn interaction_lost_reply_dispatches_once_and_reports_unknown() {
    let mut session = Session::default();
    session.acquire(&mut |_| {
        Ok(serde_json::to_vec(&json!({
            "type":"acquired", "lease":"0123456789abcdef0123456789abcdef"
        }))
        .unwrap())
    });
    let mut requests = Vec::new();
    let reply = process(
        &call(
            "handkite_text",
            json!({"expected_revision":7,"value":"fixture text"}),
        ),
        &device(),
        Capabilities::ObservationNavigationInteraction,
        &mut session,
        &mut |bytes| {
            requests.push(serde_json::from_slice::<Value>(bytes).unwrap());
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "lost reply"))
        },
    )
    .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["request"]["action"],
        json!({"type":"text","value":"fixture text"})
    );
    assert_eq!(requests[0]["request"]["expected_revision"], 7);
    assert_eq!(payload(&reply)["receipt"]["accepted"], Value::Null);
    assert_eq!(reply["result"]["isError"], true);
    assert!(!reply.to_string().contains("fixture text"));
    assert!(!reply.to_string().contains("0123456789abcdef"));
}

#[test]
fn malformed_or_forged_action_arguments_never_exchange() {
    let mut arguments = vec![json!({}), Value::Null, json!([])];
    for revision in [
        Value::Null,
        json!(-1),
        json!(1.5),
        json!(true),
        json!("1"),
        json!([]),
        json!({}),
    ] {
        arguments.push(json!({"expected_revision": revision}));
    }
    for field in [
        "owner",
        "lease",
        "token",
        "device",
        "socket",
        "capabilities",
        "ttl_ms",
        "bundle_id",
        "action",
        "text",
        "at",
    ] {
        let mut args = json!({"expected_revision": 1});
        args[field] = json!("forged");
        arguments.push(args);
    }
    for name in ["handkite_home", "handkite_launch_clock"] {
        for args in &arguments {
            let reply = process(
                &call(name, args.clone()),
                &device(),
                Capabilities::ObservationAndNavigation,
                &mut Session::default(),
                &mut |_| panic!("malformed action reached socket"),
            )
            .unwrap();
            assert_eq!(reply["error"]["code"], -32602);
        }
    }
}

#[test]
fn duplicate_fields_are_rejected_before_socket_dispatch() {
    for input in [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":1,"expected_revision":2}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_status","name":"handkite_acquire_control","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"tools/call","params":{"name":"handkite_acquire_control","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_acquire_control","arguments":{},"arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":18446744073709551616}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":1.0}}}"#,
    ] {
        let mut output = Vec::new();
        serve_with_capabilities(
            Cursor::new(format!("{input}\n")),
            &mut output,
            &device(),
            Capabilities::ObservationAndNavigation,
            |_| panic!("duplicate field reached socket"),
        )
        .unwrap();
        let reply: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(reply["error"]["code"], -32602);
    }
}

#[test]
fn socket_transcript_returns_post_action_screen_without_another_observation() {
    let mut fixture = SocketFixture::new();
    let input = frames(&[
        call("handkite_status", json!({})),
        call("handkite_acquire_control", json!({})),
        call("handkite_launch_clock", json!({"expected_revision": 1})),
        call("handkite_home", json!({"expected_revision": 3})),
        call("handkite_release_control", json!({})),
    ]);
    let (result, output) = fixture.transcript(&input, Capabilities::ObservationAndNavigation);
    result.unwrap();
    let replies: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(payload(&replies[2])["receipt"]["accepted"], true);
    assert_eq!(payload(&replies[2])["effect"], "changed");
    assert!(replies[2]["result"].get("isError").is_none());
    assert!(replies[3]["result"].get("isError").is_none());
    assert_eq!(
        payload(&replies[2])["after"]["state"]["foreground_app"],
        "com.apple.mobiletimer"
    );
    assert_eq!(
        payload(&replies[3])["after"]["state"]["foreground_app"],
        "com.apple.springboard"
    );
    for reply in &replies[2..4] {
        let content = reply["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        let screen: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(screen["metadata"], payload(reply)["after"]);
        assert_eq!(screen["accessibility_tree"], "<private-screen/>");
        assert_eq!(content[2]["type"], "image");
        assert_eq!(content[2]["data"], "cHJpdmF0ZS1zY3JlZW4=");
    }
    assert_eq!(
        fixture.state.lock().unwrap().actions,
        ["launch_app", "home"]
    );
    assert_eq!(
        fixture
            .requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["status", "acquire", "act", "act", "release"]
    );
    for request in fixture
        .requests
        .iter()
        .filter(|request| request["method"] == "act")
    {
        assert_eq!(request["request"]["device"], "fixture-phone");
        assert!(request["request"]["expected_revision"].is_u64());
    }
    let output = String::from_utf8(output).unwrap();
    for secret in fixture
        .tokens
        .iter()
        .map(String::as_str)
        .chain(["handkite-mcp"])
    {
        assert!(!output.contains(secret));
    }
}

#[test]
fn action_response_options_do_not_change_device_requests_or_receipts() {
    let mut fixture = SocketFixture::new();
    let input = frames(&[
        call("handkite_status", json!({})),
        call("handkite_acquire_control", json!({})),
        call(
            "handkite_launch_clock",
            json!({"expected_revision":1,
            "response":{"tree_format":"compact","image":"omit"}}),
        ),
        call("handkite_release_control", json!({})),
    ]);
    let (result, output) = fixture.transcript(&input, Capabilities::ObservationAndNavigation);
    result.unwrap();
    let replies: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(payload(&replies[2])["receipt"]["accepted"], true);
    let content = replies[2]["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let screen: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(screen["metadata"], payload(&replies[2])["after"]);
    assert_eq!(screen["image"]["reason"], "requested");
    assert_eq!(fixture.state.lock().unwrap().actions, ["launch_app"]);
    assert_eq!(
        fixture
            .requests
            .iter()
            .map(|r| r["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["status", "acquire", "act", "release"]
    );
    assert!(fixture.requests.iter().all(|r| r.get("response").is_none()));
}

#[test]
fn competing_sessions_are_busy_until_explicit_release() {
    let mut fixture = SocketFixture::new();
    let mut first = Session::default();
    let mut second = Session::default();
    assert_eq!(
        payload(&fixture.call(&mut first, "handkite_acquire_control", json!({})))["control"],
        "acquired"
    );
    let count = fixture.requests.len();
    assert_eq!(
        fixture.call(&mut first, "handkite_acquire_control", json!({}))["result"]["isError"],
        true
    );
    assert_eq!(fixture.requests.len(), count);
    let busy = fixture.call(&mut second, "handkite_acquire_control", json!({}));
    assert_eq!(busy["result"]["isError"], true);
    assert_eq!(busy["result"]["structuredContent"]["code"], "Busy");
    assert!(
        busy.to_string()
            .contains("Busy: another session holds control")
    );
    fixture.call(&mut first, "handkite_release_control", json!({}));
    assert_eq!(
        payload(&fixture.call(&mut second, "handkite_acquire_control", json!({})))["control"],
        "acquired"
    );
    second.cleanup(&mut |bytes| fixture.exchange(bytes));
    assert_ne!(fixture.tokens[0], fixture.tokens[1]);
}

#[test]
fn stale_revision_and_changed_view_do_not_send_input() {
    let mut fixture = SocketFixture::new();
    let mut session = Session::default();
    fixture.call(&mut session, "handkite_status", json!({}));
    fixture.call(&mut session, "handkite_acquire_control", json!({}));
    assert!(
        fixture
            .call(
                &mut session,
                "handkite_home",
                json!({"expected_revision": 0})
            )
            .to_string()
            .contains("StaleRevision")
    );
    fixture.state.lock().unwrap().app = Some("com.example.changed".into());
    let changed = fixture.call(
        &mut session,
        "handkite_home",
        json!({"expected_revision": 1}),
    );
    assert_eq!(changed["result"]["isError"], true);
    assert_eq!(
        changed["result"]["content"][0]["text"],
        "ViewChanged: screen context changed before input; obtain a fresh observation"
    );
    assert_eq!(
        changed["result"]["structuredContent"],
        json!({
            "code":"ViewChanged", "changed":{"state":true,"accessibility_tree":false,"screenshot":false}
        })
    );
    for private in ["private-screen", "com.example.changed", "fixture-usb"] {
        assert!(!changed.to_string().contains(private));
    }
    assert!(fixture.state.lock().unwrap().actions.is_empty());
    session.cleanup(&mut |bytes| fixture.exchange(bytes));
}

#[test]
fn view_change_diagnostics_are_typed_and_do_not_forward_private_fields() {
    let mut cases = Vec::new();
    for mask in 1..8 {
        cases.push((json!({"state":mask & 1 != 0,"accessibility_tree":mask & 2 != 0,"screenshot":mask & 4 != 0}), true));
    }
    for admission in ["missing", "expired", "mismatched", "present"] {
        cases.push((json!({"state":false,"accessibility_tree":false,"screenshot":true,"gesture_admission":admission}), true));
    }
    for changed in [
        json!({"state":false,"accessibility_tree":false,"screenshot":false}),
        json!({"state":"private-value","accessibility_tree":true,"screenshot":false}),
        json!({"state":true,"screenshot":false}),
        json!({"state":true,"accessibility_tree":false,"screenshot":false,"private-value":"private-screen"}),
        json!({"state":false,"accessibility_tree":false,"screenshot":true,"gesture_admission":"private-screen"}),
    ] {
        cases.push((changed, false));
    }
    for (changed, valid) in cases {
        let mut session = Session::default();
        session.acquire(&mut |_| {
            Ok(serde_json::to_vec(&json!({
                "type":"acquired","lease":"0123456789abcdef0123456789abcdef"
            }))
            .unwrap())
        });
        let mut calls = 0;
        let result = session.act(
            &device(),
            Revision(1),
            Action::Home,
            true,
            presentation::Options::default(),
            &mut |_| {
                calls += 1;
                Ok(serde_json::to_vec(&json!({"type":"view_changed","changed":changed})).unwrap())
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(result["isError"], true);
        if valid {
            assert_eq!(
                result["structuredContent"],
                json!({"code":"ViewChanged","changed":changed})
            );
        } else {
            assert!(result.get("structuredContent").is_none());
            let text = result["content"][0]["text"].as_str().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(text).unwrap()["delivery"],
                "unknown"
            );
        }
        assert!(!result.to_string().contains("private-value"));
        assert!(!result.to_string().contains("private-screen"));
    }
}

#[test]
fn expired_lease_requires_a_new_explicit_acquisition() {
    let mut fixture = SocketFixture::new();
    fixture.short_ttl = true;
    let mut session = Session::default();
    fixture.call(&mut session, "handkite_status", json!({}));
    fixture.call(&mut session, "handkite_acquire_control", json!({}));
    thread::sleep(Duration::from_millis(20));
    assert!(
        fixture
            .call(
                &mut session,
                "handkite_home",
                json!({"expected_revision": 1})
            )
            .to_string()
            .contains("InvalidLease")
    );
    let count = fixture.requests.len();
    assert_eq!(
        fixture.call(
            &mut session,
            "handkite_home",
            json!({"expected_revision": 1})
        )["result"]["isError"],
        true
    );
    assert_eq!(fixture.requests.len(), count);
    assert!(fixture.state.lock().unwrap().actions.is_empty());
    fixture.short_ttl = false;
    assert_eq!(
        payload(&fixture.call(&mut session, "handkite_acquire_control", json!({})))["control"],
        "acquired"
    );
    session.cleanup(&mut |bytes| fixture.exchange(bytes));
}

#[test]
fn valid_action_outcomes_preserve_evidence_and_flag_failures_without_replay() {
    for (capabilities, format, mime) in [
        (Capabilities::Navigation, "png", None),
        (Capabilities::Navigation, "jpeg", None),
        (
            Capabilities::ObservationAndNavigation,
            "png",
            Some("image/png"),
        ),
        (
            Capabilities::ObservationAndNavigation,
            "jpeg",
            Some("image/jpeg"),
        ),
        (
            Capabilities::ObservationNavigationInteraction,
            "png",
            Some("image/png"),
        ),
    ] {
        for (accepted, effect, is_error) in [
            (json!(true), "changed", false),
            (json!(false), "changed", true),
            (Value::Null, "changed", true),
            (json!(true), "unconfirmed", true),
            (json!(false), "unconfirmed", true),
            (Value::Null, "unconfirmed", true),
        ] {
            let mut session = Session::default();
            session.acquire(&mut |_| {
                Ok(serde_json::to_vec(&json!({
                    "type": "acquired", "lease": "0123456789abcdef0123456789abcdef"
                }))
                .unwrap())
            });
            let receipt = json!({"accepted": accepted, "action_kind": "home"});
            let after_metadata = json!({
                "device": "fixture-phone", "revision": 3,
                "window": {"started_unix_ms": 10, "finished_unix_ms": 20},
                "state": {"lock": "unlocked", "wda_ready": true,
                    "foreground_app": "com.apple.springboard"}
            });
            let mut after = after_metadata.clone();
            after["accessibility_tree"] = json!("<private-screen/>");
            after["screenshot_format"] = json!(format);
            after["screenshot_base64"] = json!("cHJpdmF0ZS1zY3JlZW4=");
            let mut requests = Vec::new();
            let reply = process(
                &call("handkite_home", json!({"expected_revision": 1})),
                &device(),
                capabilities,
                &mut session,
                &mut |bytes| {
                    requests.push(serde_json::from_slice::<Value>(bytes).unwrap());
                    Ok(serde_json::to_vec(&json!({
                        "type": "acted", "receipt": receipt, "effect": effect,
                        "after": after, "native_lock": null
                    }))
                    .unwrap())
                },
            )
            .unwrap();
            assert_eq!(
                reply["result"].get("isError"),
                is_error.then_some(&json!(true))
            );
            assert_eq!(
                payload(&reply),
                json!({"receipt": receipt, "effect": effect, "after": after_metadata})
            );
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["method"], "act");
            let content = reply["result"]["content"].as_array().unwrap();
            if let Some(mime) = mime {
                assert_eq!(content.len(), 3);
                let screen: Value =
                    serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
                assert_eq!(screen["metadata"], after_metadata);
                assert_eq!(screen["accessibility_tree"], "<private-screen/>");
                assert_eq!(content[2]["mimeType"], mime);
                assert_eq!(content[2]["data"], "cHJpdmF0ZS1zY3JlZW4=");
            } else {
                assert_eq!(content.len(), 1);
                assert!(!reply.to_string().contains("private-screen"));
                assert!(!reply.to_string().contains("cHJpdmF0ZS1zY3JlZW4="));
            }
        }
    }
}

#[test]
fn accepted_unchanged_view_and_failed_capture_are_errors_over_socket() {
    for fail_capture in [false, true] {
        let mut fixture = SocketFixture::new();
        {
            let mut state = fixture.state.lock().unwrap();
            state.app = Some("com.apple.springboard".into());
            state.fail_capture_after_action = fail_capture;
        }
        let mut session = Session::default();
        let before = payload(&fixture.call(&mut session, "handkite_status", json!({})));
        fixture.call(&mut session, "handkite_acquire_control", json!({}));
        let reply = fixture.call(
            &mut session,
            "handkite_home",
            json!({"expected_revision": 1}),
        );
        let result = payload(&reply);
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(
            result["receipt"],
            json!({"accepted": true, "action_kind": "home"})
        );
        assert_eq!(result["effect"], "unconfirmed");
        assert!(result.get("delivery").is_none());
        if fail_capture {
            assert_eq!(result["after"], Value::Null);
        } else {
            assert_eq!(result["after"]["device"], before["device"]);
            assert_eq!(result["after"]["revision"], 3);
            assert_eq!(result["after"]["state"], before["state"]);
            assert!(result["after"]["window"]["started_unix_ms"].is_u64());
            assert!(result["after"].get("screenshot_base64").is_none());
            assert!(result["after"].get("accessibility_tree").is_none());
        }
        assert_eq!(
            reply["result"]["content"].as_array().unwrap().len(),
            if fail_capture { 1 } else { 3 }
        );
        assert_eq!(fixture.state.lock().unwrap().actions, ["home"]);
        assert_eq!(
            fixture
                .requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["status", "acquire", "act"]
        );
        session.cleanup(&mut |bytes| fixture.exchange(bytes));
    }
}

#[test]
fn uncertain_action_delivery_is_reported_without_replay() {
    for lose_socket_reply in [true, false] {
        let mut fixture = SocketFixture::new();
        let mut session = Session::default();
        fixture.call(&mut session, "handkite_status", json!({}));
        fixture.call(&mut session, "handkite_acquire_control", json!({}));
        if lose_socket_reply {
            fixture.lose_reply = Some("act");
        } else {
            fixture.state.lock().unwrap().fail_action = true;
        }
        let reply = fixture.call(
            &mut session,
            "handkite_home",
            json!({"expected_revision": 1}),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(payload(&reply)["receipt"]["accepted"], Value::Null);
        assert_eq!(payload(&reply)["effect"], "unconfirmed");
        assert_eq!(payload(&reply)["delivery"], "unknown");
        assert_eq!(
            reply["result"]["structuredContent"]["code"],
            if lose_socket_reply {
                Value::Null
            } else {
                json!("Adapter")
            }
        );
        assert_eq!(fixture.state.lock().unwrap().actions, ["home"]);
        assert_eq!(
            fixture
                .requests
                .iter()
                .filter(|request| request["method"] == "act")
                .count(),
            1
        );
        session.cleanup(&mut |bytes| fixture.exchange(bytes));
    }
}

#[test]
fn post_input_identity_and_response_size_failures_report_unknown_delivery() {
    for mismatch in [true, false] {
        let mut fixture = SocketFixture::new();
        let mut session = Session::default();
        fixture.call(&mut session, "handkite_status", json!({}));
        fixture.call(&mut session, "handkite_acquire_control", json!({}));
        {
            let mut state = fixture.state.lock().unwrap();
            state.mismatch_after_action = mismatch;
            state.oversized_after_action = !mismatch;
        }
        let reply = fixture.call(
            &mut session,
            "handkite_home",
            json!({"expected_revision": 1}),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(payload(&reply)["receipt"]["accepted"], Value::Null);
        assert_eq!(payload(&reply)["delivery"], "unknown");
        assert_eq!(fixture.state.lock().unwrap().actions, ["home"]);
        assert_eq!(
            fixture
                .requests
                .iter()
                .filter(|request| request["method"] == "act")
                .count(),
            1
        );
        session.cleanup(&mut |bytes| fixture.exchange(bytes));
    }
}

#[test]
fn lost_acquisition_reply_never_reacquires_or_exposes_the_token() {
    let mut fixture = SocketFixture::new();
    fixture.lose_reply = Some("acquire");
    let mut session = Session::default();
    let first = fixture.call(&mut session, "handkite_acquire_control", json!({}));
    let count = fixture.requests.len();
    let second = fixture.call(&mut session, "handkite_acquire_control", json!({}));
    session.cleanup(&mut |bytes| fixture.exchange(bytes));
    assert_eq!(fixture.requests.len(), count);
    assert_eq!(first["result"]["isError"], true);
    assert_eq!(second["result"]["isError"], true);
    assert!(!first.to_string().contains(&fixture.tokens[0]));
}

#[test]
fn eof_and_input_errors_release_control() {
    for suffix in [
        vec![],
        b"{\"partial\":".to_vec(),
        vec![b'x'; MAX_REQUEST + 1],
    ] {
        let mut fixture = SocketFixture::new();
        let mut input = frames(&[call("handkite_acquire_control", json!({}))]);
        input.extend_from_slice(&suffix);
        let (result, _) = fixture.transcript(&input, Capabilities::Navigation);
        assert_eq!(result.is_ok(), suffix.is_empty());
        assert_eq!(
            fixture
                .requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["acquire", "release"]
        );
        let mut next = Session::default();
        assert_eq!(
            payload(&fixture.call(&mut next, "handkite_acquire_control", json!({})))["control"],
            "acquired"
        );
        next.cleanup(&mut |bytes| fixture.exchange(bytes));
    }
}

#[test]
fn output_failure_still_releases_control() {
    struct BrokenOutput;
    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut fixture = SocketFixture::new();
    let input = frames(&[call("handkite_acquire_control", json!({}))]);
    assert!(
        serve_with_capabilities(
            Cursor::new(input),
            BrokenOutput,
            &device(),
            Capabilities::Navigation,
            |bytes| fixture.exchange(bytes)
        )
        .is_err()
    );
    assert_eq!(
        fixture
            .requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["acquire", "release"]
    );
}

#[test]
fn unexpected_service_replies_cannot_disclose_tokens() {
    let token = "0123456789abcdef0123456789abcdef";
    let mut session = Session::default();
    let mut replies = vec![
        json!({"type": "acquired", "lease": token}),
        json!({"type": "error", "code": token}),
        json!({"type": "acted", "receipt": {"accepted": true, "action_kind": token}, "after": null, "native_lock": null, "effect": "changed"}),
        json!({"type": "acquired", "lease": token}),
        json!({"type": "released"}),
    ].into_iter();
    let mut exchange = |_: &[u8]| Ok(serde_json::to_vec(&replies.next().unwrap()).unwrap());
    let mut outputs = vec![session.acquire(&mut exchange)];
    for _ in 0..3 {
        outputs.push(session.act(
            &device(),
            Revision(1),
            Action::Home,
            true,
            presentation::Options::default(),
            &mut exchange,
        ));
    }
    outputs.push(session.release(&mut exchange));
    assert!(outputs[1..4].iter().all(|reply| reply["isError"] == true));
    assert!(
        outputs
            .iter()
            .all(|reply| !reply.to_string().contains(token))
    );
    assert!(replies.next().is_none());
}

#[test]
fn release_failure_retains_cleanup_attempt_without_reacquiring() {
    let mut fixture = SocketFixture::new();
    let mut session = Session::default();
    fixture.call(&mut session, "handkite_acquire_control", json!({}));
    fixture.lose_reply = Some("release");
    assert_eq!(
        fixture.call(&mut session, "handkite_release_control", json!({}))["result"]["isError"],
        true
    );
    session.cleanup(&mut |bytes| fixture.exchange(bytes));
    assert_eq!(
        fixture
            .requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["acquire", "release", "release"]
    );
}
