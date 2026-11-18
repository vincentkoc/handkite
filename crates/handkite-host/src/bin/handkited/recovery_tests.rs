use super::*;
use handkite_core::controller::{
    AdapterError, ControllerError, RawImageObservation, RawObservation,
};
use handkite_core::protocol::{
    Action, DeviceState, ImageFormat, LockState, ObservationWindow, ScreenImage, TransportReceipt,
};
use std::cell::Cell;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct State {
    exited: AtomicBool,
    shutdown: AtomicBool,
    observations: AtomicUsize,
    image_observations: AtomicUsize,
    actions: AtomicUsize,
    action_finished: AtomicBool,
    cleaned: AtomicBool,
}

struct Adapter {
    state: Arc<State>,
    exit_on_observe: bool,
    fail_observe: bool,
    fail_image: bool,
    shutdown_on_drop: bool,
}

fn hardware_id() -> HardwareId {
    HardwareId::new("fixture-usb").unwrap()
}

fn key() -> DeviceKey {
    DeviceKey::new("fixture").unwrap()
}

impl DeviceAdapter for Adapter {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        Ok((hardware_id(), hardware_id()))
    }

    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.state.observations.fetch_add(1, SeqCst);
        if self.exit_on_observe {
            self.state.exited.store(true, SeqCst);
        }
        if self.fail_observe || self.state.exited.load(SeqCst) {
            return Err(AdapterError);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Ok(RawObservation {
            connected_usb: hardware_id(),
            wda_target: hardware_id(),
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some("com.example.fixture".into()),
            },
            accessibility_tree: "<fixture/>".into(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![],
            },
        })
    }

    fn observe_image(&mut self) -> Result<RawImageObservation, AdapterError> {
        self.state.observations.fetch_add(1, SeqCst);
        self.state.image_observations.fetch_add(1, SeqCst);
        if self.exit_on_observe {
            self.state.exited.store(true, SeqCst);
        }
        if self.fail_image || self.state.exited.load(SeqCst) {
            return Err(AdapterError);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Ok(RawImageObservation {
            connected_usb: hardware_id(),
            wda_target: hardware_id(),
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![1],
            },
        })
    }

    fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
        self.state.actions.fetch_add(1, SeqCst);
        self.state.exited.store(true, SeqCst);
        self.state.action_finished.store(true, SeqCst);
        // Delivery became uncertain while the action was in progress.
        Err(AdapterError)
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        if self.state.actions.load(SeqCst) > 0 {
            assert!(self.state.action_finished.load(SeqCst));
        }
        // Normal owned cleanup also finishes the task; this must not cause failure.
        self.state.exited.store(true, SeqCst);
        if self.shutdown_on_drop {
            self.state.shutdown.store(true, SeqCst);
        }
        self.state.cleaned.store(true, SeqCst);
    }
}

fn adapter(state: &Arc<State>) -> Adapter {
    Adapter {
        state: Arc::clone(state),
        exit_on_observe: false,
        fail_observe: false,
        fail_image: false,
        shutdown_on_drop: false,
    }
}

fn controller(adapter: Adapter) -> Controller<Adapter> {
    Controller::new(
        DeviceBinding {
            key: key(),
            usb_id: hardware_id(),
        },
        adapter,
        5000,
        5000,
    )
}

fn assert_lost(result: Result<(), String>, state: &State) {
    assert_eq!(result, Err("owned_runner_exited".into()));
    assert!(state.cleaned.load(SeqCst));
}

#[test]
fn runner_loss_before_readiness_skips_observation_and_serving() {
    let state = Arc::new(State::default());
    state.exited.store(true, SeqCst);
    let result = serve_controller(
        controller(adapter(&state)),
        &key(),
        || false,
        || state.exited.load(SeqCst),
        |_, _| panic!("must not serve"),
    );
    assert_lost(result, &state);
    assert_eq!(state.observations.load(SeqCst), 0);
}

#[test]
fn runner_loss_during_readiness_has_the_fixed_exit_reason() {
    let state = Arc::new(State::default());
    let mut adapter = adapter(&state);
    adapter.exit_on_observe = true;
    let result = serve_controller(
        controller(adapter),
        &key(),
        || false,
        || state.exited.load(SeqCst),
        |_, _| panic!("must not announce readiness or serve"),
    );
    assert_lost(result, &state);
    assert_eq!(state.observations.load(SeqCst), 1);
}

#[test]
fn ordinary_readiness_failure_keeps_its_reason_after_cleanup() {
    let state = Arc::new(State::default());
    let mut adapter = adapter(&state);
    adapter.fail_image = true;
    let result = serve_controller(
        controller(adapter),
        &key(),
        || false,
        || state.exited.load(SeqCst),
        |_, _| panic!("must not serve"),
    );
    assert_eq!(result, Err("pinned device is not ready: Adapter".into()));
    assert!(state.cleaned.load(SeqCst));
}

#[test]
fn changing_application_tree_does_not_block_readiness_or_bypass_later_capture() {
    let state = Arc::new(State::default());
    let mut adapter = adapter(&state);
    adapter.fail_observe = true;
    let result = serve_controller(
        controller(adapter),
        &key(),
        || false,
        || state.exited.load(SeqCst),
        |controller, _| {
            assert_eq!(state.image_observations.load(SeqCst), 1);
            assert_eq!(state.observations.load(SeqCst), 1);
            assert!(matches!(
                controller.observe(&key()),
                Err(ControllerError::Adapter)
            ));
            assert_eq!(state.observations.load(SeqCst), 2);
            assert_eq!(state.actions.load(SeqCst), 0);
            Ok(())
        },
    );
    assert_eq!(result, Ok(()));
    assert!(state.cleaned.load(SeqCst));
}

#[test]
fn normal_cleanup_and_borrowed_serving_do_not_report_runner_loss() {
    for borrowed in [false, true] {
        let state = Arc::new(State::default());
        let result = serve_controller(
            controller(adapter(&state)),
            &key(),
            || false,
            || !borrowed && state.exited.load(SeqCst),
            |_, stop| {
                if borrowed {
                    state.exited.store(true, SeqCst);
                }
                assert!(!stop());
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert!(state.cleaned.load(SeqCst));
    }
}

#[test]
fn intentional_shutdown_wins_even_when_observed_during_cleanup() {
    for during_cleanup in [false, true] {
        let state = Arc::new(State::default());
        let mut adapter = adapter(&state);
        adapter.shutdown_on_drop = during_cleanup;
        if !during_cleanup {
            state.shutdown.store(true, SeqCst);
        }
        state.exited.store(true, SeqCst);
        let result = serve_controller(
            controller(adapter),
            &key(),
            || state.shutdown.load(SeqCst),
            || state.exited.load(SeqCst),
            |_, _| panic!("must not serve"),
        );
        assert_eq!(result, Ok(()));
        assert!(state.cleaned.load(SeqCst));
        assert_eq!(state.observations.load(SeqCst), 0);
    }
}

#[test]
fn runner_loss_stops_idle_and_partial_clients_without_device_polling() {
    for partial in [false, true] {
        let state = Arc::new(State::default());
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let _client = partial.then(|| {
            let mut client = UnixStream::connect(&socket).unwrap();
            client.write_all(b"{").unwrap();
            client
        });
        let started = Instant::now();
        let result = serve_controller(
            controller(adapter(&state)),
            &key(),
            || false,
            || state.exited.load(SeqCst),
            |controller, stop| {
                let signal = Arc::clone(&state);
                let worker = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(40));
                    signal.exited.store(true, SeqCst);
                });
                let result = local_socket::serve_until(listener, controller, stop);
                worker.join().unwrap();
                result.map_err(|_| "local socket failed".into())
            },
        );
        assert_lost(result, &state);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(state.observations.load(SeqCst), 1);
        assert_eq!(state.actions.load(SeqCst), 0);
    }
}

#[test]
fn runner_loss_after_accept_discards_the_queued_request() {
    let state = Arc::new(State::default());
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .write_all(b"{\"method\":\"observe\",\"device\":\"fixture\"}\n")
        .unwrap();
    let serving = Cell::new(false);
    let checks = Cell::new(0);
    let result = serve_controller(
        controller(adapter(&state)),
        &key(),
        || false,
        || {
            if serving.get() {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    state.exited.store(true, SeqCst);
                }
            }
            state.exited.load(SeqCst)
        },
        |controller, stop| {
            serving.set(true);
            local_socket::serve_until(listener, controller, stop)
                .map_err(|_| "local socket failed".into())
        },
    );
    assert_lost(result, &state);
    assert_eq!(state.observations.load(SeqCst), 1);
    assert_eq!(state.actions.load(SeqCst), 0);
}

#[test]
fn runner_loss_during_action_finishes_once_without_replay_or_queued_dispatch() {
    let state = Arc::new(State::default());
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut first = UnixStream::connect(&socket).unwrap();
    let mut queued = UnixStream::connect(&socket).unwrap();
    let result = serve_controller(
        controller(adapter(&state)),
        &key(),
        || false,
        || state.exited.load(SeqCst),
        |controller, stop| {
            let lease = controller
                .acquire("fixture".into(), Duration::from_secs(5))
                .unwrap();
            let mut request = serde_json::to_vec(&serde_json::json!({
                "method": "act", "owner": "fixture", "lease": String::from(lease),
                "request": {"device": "fixture", "expected_revision": null, "action": {"type": "home"}}
            })).unwrap();
            request.push(b'\n');
            first.write_all(&request).unwrap();
            queued.write_all(&request).unwrap();
            local_socket::serve_until(listener, controller, stop)
                .map_err(|_| "local socket failed".into())
        },
    );
    assert_lost(result, &state);
    assert_eq!(state.actions.load(SeqCst), 1);
    assert!(state.action_finished.load(SeqCst));
    assert_eq!(state.observations.load(SeqCst), 2);
}
