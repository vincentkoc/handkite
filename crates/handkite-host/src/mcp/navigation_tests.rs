use super::*;
use crate::{local_client, local_socket};
use handkite_core::controller::{AdapterError, Controller, DeviceAdapter, RawObservation};
use handkite_core::protocol::{
    DeviceBinding, DeviceState, HardwareId, LockState, ObservationWindow, ScreenImage,
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
    for (capabilities, expected) in [
        (Capabilities::Observation, observation.to_vec()),
        (Capabilities::Navigation, navigation.to_vec()),
        (
            Capabilities::ObservationAndNavigation,
            [observation.as_slice(), navigation.as_slice()].concat(),
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
        for name in observation.iter().chain(navigation.iter()).chain(
            [
                "handkite_act",
                "handkite_tap",
                "handkite_swipe",
                "handkite_text",
                "handkite_lock",
                "handkite_unlock",
            ]
            .iter(),
        ) {
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
fn socket_transcript_navigates_and_never_discloses_control_or_screen_content() {
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
    assert_eq!(
        payload(&replies[2])["after"]["state"]["foreground_app"],
        "com.apple.mobiletimer"
    );
    assert_eq!(
        payload(&replies[3])["after"]["state"]["foreground_app"],
        "com.apple.springboard"
    );
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
        ["observe", "acquire", "act", "act", "release"]
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
    for secret in fixture.tokens.iter().map(String::as_str).chain([
        "handkite-mcp",
        "<private-screen/>",
        "cHJpdmF0ZS1zY3JlZW4=",
        "screenshot",
        "accessibility_tree",
    ]) {
        assert!(!output.contains(secret));
    }
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
    assert!(
        fixture
            .call(&mut second, "handkite_acquire_control", json!({}))
            .to_string()
            .contains("Busy")
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
    assert!(
        fixture
            .call(
                &mut session,
                "handkite_home",
                json!({"expected_revision": 1})
            )
            .to_string()
            .contains("StaleRevision")
    );
    assert!(fixture.state.lock().unwrap().actions.is_empty());
    session.cleanup(&mut |bytes| fixture.exchange(bytes));
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
        assert_eq!(payload(&reply)["receipt"]["accepted"], Value::Null);
        assert_eq!(payload(&reply)["effect"], "unconfirmed");
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
        outputs.push(session.act(&device(), Revision(1), Action::Home, &mut exchange));
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
