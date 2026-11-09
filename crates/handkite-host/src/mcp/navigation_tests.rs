use super::*;
use crate::{local_client, local_socket};
use handkite_core::controller::{AdapterError, Controller, DeviceAdapter, RawObservation};
use handkite_core::protocol::{DeviceBinding, HardwareId, TransportReceipt};
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct FakeAdapter;
impl DeviceAdapter for FakeAdapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        Err(AdapterError)
    }
    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        Err(AdapterError)
    }
    fn execute(
        &mut self,
        _: &handkite_core::protocol::Action,
    ) -> Result<TransportReceipt, AdapterError> {
        panic!("session lease tools must never send device input")
    }
}

struct SocketFixture {
    directory: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
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
        let controller = Controller::new(
            DeviceBinding {
                key: device(),
                usb_id: HardwareId::new("fixture-usb").unwrap(),
            },
            FakeAdapter,
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
    let navigation = ["handkite_acquire_control", "handkite_release_control"];
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
                "handkite_home",
                "handkite_launch_clock",
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
