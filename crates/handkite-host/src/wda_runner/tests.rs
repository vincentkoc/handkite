use super::*;
use handkite_core::protocol::HardwareId;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

struct OnDrop(Arc<AtomicUsize>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn ready() -> Option<HttpResponse> {
    Some(HttpResponse {
        status: 200,
        body: br#"{"value":{"ready":true}}"#.to_vec(),
    })
}

async fn live(mut listener: PlanListener, dropped: Arc<AtomicUsize>) -> Result<(), RunnerError> {
    let _guard = OnDrop(dropped);
    listener.did_begin_executing_test_plan().await.unwrap();
    pending().await
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[tokio::test(start_paused = true)]
async fn authorization_wait_survives_sixty_seconds_without_claiming_readiness() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let probes = AtomicUsize::new(0);
    let started = Instant::now();
    let runner = OwnedWdaRunner::start_task(
        |mut listener| {
            let dropped = dropped.clone();
            async move {
                let _guard = OnDrop(dropped);
                listener
                    .log_debug_message("entering wait loop for enabling automation mode")
                    .await
                    .unwrap();
                sleep(Duration::from_secs(90)).await;
                listener
                    .log_debug_message("cleaning up enabling automation mode")
                    .await
                    .unwrap();
                listener.did_begin_executing_test_plan().await.unwrap();
                pending().await
            }
        },
        || {
            probes.fetch_add(1, Ordering::SeqCst);
            async { ready() }
        },
        deadline(),
    )
    .await
    .expect("owner authorization within the bounded window must reach readiness");
    assert_eq!(Instant::now() - started, Duration::from_secs(90));
    assert_eq!(runner.ready_deadline, started + Duration::from_secs(180));
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    runner
        .abort_and_join_with(|| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn repeated_authorization_notices_do_not_renew_the_window() {
    let started = Instant::now();
    let probes = AtomicUsize::new(0);
    let result = OwnedWdaRunner::start_task(
        |mut listener| async move {
            for delay in [0, 100] {
                sleep(Duration::from_secs(delay)).await;
                listener
                    .log_debug_message("entering wait loop for enabling automation mode")
                    .await
                    .unwrap();
            }
            pending().await
        },
        || {
            probes.fetch_add(1, Ordering::SeqCst);
            async { ready() }
        },
        deadline(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::AuthorizationRequired));
    assert_eq!(Instant::now() - started, Duration::from_secs(180));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn first_authorization_notice_cannot_rescue_expired_or_running_startup() {
    for (phase, delay) in [
        (PlanPhase::Starting, 30),
        (PlanPhase::Starting, 31),
        (PlanPhase::Running, 0),
        (PlanPhase::Finished, 0),
    ] {
        let initial_deadline = Instant::now() + Duration::from_secs(30);
        let (sender, receiver) = watch::channel(PlanState {
            phase,
            ..PlanState::starting()
        });
        let mut listener = PlanListener {
            phase: sender,
            initial_deadline,
        };
        sleep(Duration::from_secs(delay)).await;
        listener
            .log_debug_message("entering wait loop for enabling automation mode")
            .await
            .unwrap();
        let state = *receiver.borrow();
        assert!(state.authorization_deadline.is_none());
        assert!(!state.automation_waiting);
        assert_eq!(
            effective_startup_deadline(state, initial_deadline),
            initial_deadline
        );
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_errors_still_end_an_authorization_wait_immediately() {
    for pairing in [false, true] {
        let started = Instant::now();
        let result = OwnedWdaRunner::start_task(
            |mut listener| async move {
                listener
                    .log_debug_message("entering wait loop for enabling automation mode")
                    .await
                    .unwrap();
                sleep(Duration::from_secs(70)).await;
                listener
                    .initialization_for_ui_testing_did_fail(if pairing {
                        "InvalidHostID"
                    } else {
                        "Error Domain=com.apple.LocalAuthentication Code=-2"
                    })
                    .await
                    .unwrap();
                pending().await
            },
            || async { panic!("failed authorization cannot probe readiness") },
            deadline(),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(if pairing {
                RunnerError::PairingRequired
            } else {
                RunnerError::AutomationInitializationFailed
            })
        );
        assert_eq!(Instant::now() - started, Duration::from_secs(70));
    }
}

#[test]
fn authorization_timeout_is_a_runner_argument_without_changing_environment_or_filters() {
    let config = TestConfig {
        runner_bundle_id: "com.example.Runner.xctrunner".into(),
        runner_app_path: "/fixture/Runner.app".into(),
        runner_app_container: "/fixture/container".into(),
        runner_bundle_executable: "Runner-Runner".into(),
        target_bundle_id: None,
        target_app_path: None,
        target_app_env: None,
        target_app_args: None,
        tests_to_run: Some(vec!["ExistingTest".into()]),
        tests_to_skip: None,
        runner_env: None,
        runner_args: Some(vec!["existing-argument".into()]),
    };
    let updated = authorization_runner_config(config);
    assert_eq!(
        updated.runner_args.unwrap(),
        ["existing-argument", "-XCUIApplicationStateTimeout", "180",]
    );
    assert_eq!(updated.tests_to_run.unwrap(), ["ExistingTest"]);
    assert!(updated.runner_env.is_none());
    assert!(updated.target_app_env.is_none());
    assert!(updated.target_app_args.is_none());
}

#[tokio::test(start_paused = true)]
async fn explicit_pairing_requests_stop_before_readiness_or_timeout() {
    for mode in 0..3 {
        let dropped = Arc::new(AtomicUsize::new(0));
        let probes = AtomicUsize::new(0);
        let started = Instant::now();
        let result = OwnedWdaRunner::start_task(
            |mut listener| {
                let dropped = dropped.clone();
                async move {
                    let _guard = OnDrop(dropped);
                    match mode {
                        0 => listener
                            .log_debug_message(
                                "PairingDialogResponsePending sensitive-fixture-token",
                            )
                            .await
                            .unwrap(),
                        1 => listener
                            .initialization_for_ui_testing_did_fail(
                                "device is not paired sensitive-fixture-token",
                            )
                            .await
                            .unwrap(),
                        _ => {
                            let _ = listener.did_fail_to_bootstrap("InvalidHostID").await;
                        }
                    }
                    pending().await
                }
            },
            || {
                probes.fetch_add(1, Ordering::SeqCst);
                async { ready() }
            },
            deadline(),
        )
        .await;
        assert_eq!(result.err(), Some(RunnerError::PairingRequired));
        assert_eq!(Instant::now(), started);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn pairing_transport_errors_keep_other_failures_distinct() {
    for error in [
        IdeviceError::InvalidHostID,
        IdeviceError::UnknownErrorType("PairingDialogResponsePending".into()),
        IdeviceError::UnknownErrorType("UserDeniedPairing".into()),
        IdeviceError::UnexpectedResponse("missing PairRecordData in pair record response".into()),
    ] {
        assert_eq!(
            transport_failure(&error, RunnerError::LookupFailed),
            RunnerError::PairingRequired,
        );
    }
    for error in [
        IdeviceError::DeviceLocked,
        IdeviceError::DeveloperModeNotEnabled,
        IdeviceError::UnknownErrorType("unknown sensitive-fixture-token".into()),
    ] {
        assert_eq!(
            transport_failure(&error, RunnerError::LookupFailed),
            RunnerError::LookupFailed,
        );
    }
    for message in [
        "Timed out while enabling automation mode",
        "Error Domain=com.apple.LocalAuthentication Code=-4",
    ] {
        assert_eq!(
            initialization_failure(message, true),
            if message == "Timed out while enabling automation mode" {
                RunnerError::AuthorizationRequired
            } else {
                RunnerError::AutomationInitializationFailed
            },
        );
    }
    assert!(!pairing_required("Trust This Computer dialog dismissed"));
    assert!(!pairing_required(
        "Trust This Computer accepted successfully"
    ));
}

#[tokio::test]
async fn only_explicit_usbmux_refusal_admits_vacant_endpoint() {
    let deadline = deadline();
    assert_eq!(
        require_port_vacant(
            async { Err::<(), _>(IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused)) },
            deadline,
        )
        .await,
        Ok(())
    );
    assert_eq!(
        require_port_vacant(async { Ok::<(), IdeviceError>(()) }, deadline).await,
        Err(RunnerError::EndpointBusy)
    );
    for error in [
        IdeviceError::Usbmuxd(UsbmuxdError::BadDevice),
        IdeviceError::Socket(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
    ] {
        assert_eq!(
            require_port_vacant(async { Err::<(), _>(error) }, deadline).await,
            Err(RunnerError::EndpointUnproven)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn vacancy_timeout_and_expired_startup_refuse() {
    assert_eq!(
        require_port_vacant(pending::<Result<(), IdeviceError>>(), deadline()).await,
        Err(RunnerError::EndpointUnproven)
    );
    for delay in [VACANCY_TIMEOUT, VACANCY_TIMEOUT + Duration::from_millis(1)] {
        assert_eq!(
            require_port_vacant(
                async {
                    sleep(delay).await;
                    Err::<(), _>(IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused))
                },
                deadline(),
            )
            .await,
            Err(RunnerError::EndpointUnproven)
        );
    }
    assert_eq!(
        require_port_vacant(
            async { Err::<(), _>(IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused)) },
            Instant::now(),
        )
        .await,
        Err(RunnerError::StartupTimedOut)
    );
}

#[tokio::test]
async fn read_only_liveness_survives_handoff_to_blocking_worker() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    let liveness = runner.liveness();
    let worker = tokio::task::spawn_blocking(move || liveness);
    let liveness = worker.await.unwrap();
    assert!(!liveness.is_finished());
    runner
        .abort_and_join_with(|| async { Ok(()) })
        .await
        .unwrap();
    assert!(liveness.is_finished());
}

#[tokio::test]
async fn settings_write_admission_tracks_ownership_and_runner_liveness() {
    use std::sync::atomic::AtomicBool;
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    assert!(runner.write_admission().is_none());
    let owned = Arc::new(AtomicBool::new(true));
    let guard_owned = Arc::clone(&owned);
    runner.ownership = Some(Arc::new(move || guard_owned.load(Ordering::SeqCst)));
    let admission = runner.write_admission().unwrap();
    assert!(admission());
    owned.store(false, Ordering::SeqCst);
    assert!(!admission());
    owned.store(true, Ordering::SeqCst);
    assert!(admission());
    runner
        .abort_and_join_with(|| async { Ok(()) })
        .await
        .unwrap();
    assert!(!admission());
}

#[tokio::test(start_paused = true)]
async fn public_start_rejects_invalid_inputs_before_device_lookup() {
    let usb = PinnedUsb::new(HardwareId::new("test-device").unwrap());
    for bundle in [
        "",
        "com.example.Runner",
        "com.example.xctrunner ",
        "com..xctrunner",
        "com.example/xctrunner",
        "com.example.*.xctrunner",
        "com.example.\nxctrunner",
        "com.ex\u{00e4}mple.xctrunner",
        "example.xctrunner",
    ] {
        assert_eq!(
            OwnedWdaRunner::start(usb.clone(), bundle, Duration::from_secs(1), || true)
                .await
                .err(),
            Some(RunnerError::InvalidBundleId)
        );
    }
    assert!(validate_runner_bundle_id(&format!("com.{}.xctrunner", "a".repeat(255))).is_err());
    assert_eq!(
        validate_runner_bundle_id("com.example.WDA-Runner.xctrunner"),
        Ok(())
    );
    for timeout in [
        Duration::ZERO,
        MAX_STARTUP_TIMEOUT + Duration::from_nanos(1),
    ] {
        assert_eq!(
            OwnedWdaRunner::start(usb.clone(), "com.example.xctrunner", timeout, || true)
                .await
                .err(),
            Some(RunnerError::InvalidTimeout)
        );
    }
}

#[tokio::test]
async fn lost_owner_refuses_startup_before_device_access() {
    assert!(matches!(
        OwnedWdaRunner::start(
            PinnedUsb::new(HardwareId::new("fixture-device").unwrap()),
            "com.example.xctrunner",
            Duration::from_secs(1),
            || false,
        )
        .await,
        Err(RunnerError::OwnershipLost)
    ));
}

#[tokio::test(start_paused = true)]
async fn stale_ready_endpoint_cannot_replace_this_launchs_plan_callback() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let probes = AtomicUsize::new(0);
    let result = OwnedWdaRunner::start_task(
        |mut listener| {
            let dropped = dropped.clone();
            async move {
                let _guard = OnDrop(dropped);
                // Neither of these upstream callbacks establishes a begun plan.
                listener.test_bundle_ready().await.unwrap();
                listener
                    .test_runner_ready_with_capabilities()
                    .await
                    .unwrap();
                pending().await
            }
        },
        || {
            probes.fetch_add(1, Ordering::SeqCst);
            async { ready() }
        },
        deadline(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::PlanStartUnproven));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn automation_wait_is_bounded_and_done_does_not_establish_plan() {
    for done in [false, true] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let probes = AtomicUsize::new(0);
        let started = Instant::now();
        let expires = started + AUTHORIZATION_WINDOW;
        let result = OwnedWdaRunner::start_task(
            |mut listener| {
                let dropped = dropped.clone();
                async move {
                    let _guard = OnDrop(dropped);
                    listener
                        .log_debug_message("entering wait loop for enabling automation mode")
                        .await
                        .unwrap();
                    if done {
                        listener
                            .log_debug_message("cleaning up enabling automation mode")
                            .await
                            .unwrap();
                    }
                    pending().await
                }
            },
            || {
                probes.fetch_add(1, Ordering::SeqCst);
                async { ready() }
            },
            deadline(),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(if done {
                RunnerError::PlanStartUnproven
            } else {
                RunnerError::AuthorizationRequired
            })
        );
        assert_eq!(Instant::now(), expires);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_startup_failures_are_terminal_and_hide_raw_messages() {
    for mode in 0..3 {
        let dropped = Arc::new(AtomicUsize::new(0));
        let probes = AtomicUsize::new(0);
        let result = OwnedWdaRunner::start_task(
            |mut listener| {
                let dropped = dropped.clone();
                async move {
                    let _guard = OnDrop(dropped);
                    if mode == 2 {
                        let _ = listener
                            .did_fail_to_bootstrap("sensitive-fixture-token")
                            .await;
                    } else if mode == 1 {
                        listener
                            .log_debug_message(
                                "Failed to initialize for UI testing: sensitive-fixture-token",
                            )
                            .await
                            .unwrap();
                    } else {
                        listener
                            .initialization_for_ui_testing_did_fail("sensitive-fixture-token")
                            .await
                            .unwrap();
                    }
                    listener.did_begin_executing_test_plan().await.unwrap();
                    pending().await
                }
            },
            || {
                probes.fetch_add(1, Ordering::SeqCst);
                async { ready() }
            },
            deadline(),
        )
        .await;
        let error = result.err().unwrap();
        assert_eq!(
            error,
            if mode == 2 {
                RunnerError::BootstrapFailed
            } else {
                RunnerError::InitializationFailed
            }
        );
        assert!(!format!("{error:?}").contains("sensitive-fixture-token"));
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn task_exit_preserves_failure_after_automation_wait() {
    for failure in [
        None,
        Some(RunnerError::RunnerFailed),
        Some(RunnerError::LookupTimedOut),
    ] {
        let probes = AtomicUsize::new(0);
        let result = OwnedWdaRunner::start_task(
            |mut listener| async move {
                listener
                    .log_debug_message("entering wait loop for enabling automation mode")
                    .await
                    .unwrap();
                failure.map_or(Ok(()), Err)
            },
            || {
                probes.fetch_add(1, Ordering::SeqCst);
                async { ready() }
            },
            deadline(),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(failure.unwrap_or(RunnerError::RunnerExited))
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn automation_initialization_failures_are_distinct_from_wait_timeouts() {
    for mode in 0..4 {
        let probes = AtomicUsize::new(0);
        let result = OwnedWdaRunner::start_task(
            move |mut listener| async move {
                match mode {
                    0 => listener
                        .log_debug_message(
                            "Failed to initialize for UI testing: Error Domain=com.apple.LocalAuthentication Code=-4",
                        )
                        .await
                        .unwrap(),
                    1 => listener
                        .initialization_for_ui_testing_did_fail(
                            "Timed out while enabling automation mode",
                        )
                        .await
                        .unwrap(),
                    2 => {
                        listener
                            .log_debug_message("entering wait loop for enabling automation mode")
                            .await
                            .unwrap();
                        listener
                            .initialization_for_ui_testing_did_fail("opaque archive")
                            .await
                            .unwrap();
                    }
                    _ => listener
                        .log_message(
                            "Failed to initialize for UI testing: Error Domain=com.apple.LocalAuthentication Code=-2",
                        )
                        .await
                        .unwrap(),
                }
                pending().await
            },
            || {
                probes.fetch_add(1, Ordering::SeqCst);
                async { ready() }
            },
            deadline(),
        )
        .await;
        assert_eq!(
            result.err(),
            Some(if mode == 1 {
                RunnerError::AuthorizationRequired
            } else {
                RunnerError::AutomationInitializationFailed
            })
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn initialization_diagnostic_keeps_native_code_without_private_description() {
    assert_eq!(
        initialization_diagnostic(
            "Failed to initialize for UI testing: Error Domain=com.apple.LocalAuthentication Code=-4 UserInfo={description=sensitive-fixture-token}",
            true,
        ),
        "runner_initialization_failure automation_waiting=true local_authentication=true local_authentication_code=Some(-4)",
    );
    for message in [
        "Error Domain=other Code=-4 sensitive-fixture-token",
        "Error Domain=com.apple.LocalAuthentication Code=-4sensitive-fixture-token",
        "Error Domain=com.apple.LocalAuthentication Code=2147483648 sensitive-fixture-token",
        "sensitive-fixture-token",
    ] {
        let diagnostic = initialization_diagnostic(message, false);
        assert!(!diagnostic.contains("sensitive-fixture-token"));
        assert!(diagnostic.ends_with("local_authentication_code=None"));
    }
}

#[tokio::test]
async fn initialization_failure_during_status_probe_keeps_its_category() {
    let (release, released) = oneshot::channel();
    let mut release = Some(release);
    let result = OwnedWdaRunner::start_task(
        |mut listener| async move {
            listener.did_begin_executing_test_plan().await.unwrap();
            let _ = released.await;
            listener
                .log_debug_message("Failed to initialize for UI testing: sensitive-fixture-token")
                .await
                .unwrap();
            Err(RunnerError::RunnerFailed)
        },
        || {
            release.take().unwrap().send(()).unwrap();
            async {
                tokio::task::yield_now().await;
                ready()
            }
        },
        deadline(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::InitializationFailed));
}

#[tokio::test]
async fn completed_automation_wait_can_reach_a_proven_plan() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |mut listener| {
            let dropped = dropped.clone();
            async move {
                let _guard = OnDrop(dropped);
                listener
                    .log_debug_message("entering wait loop for enabling automation mode")
                    .await
                    .unwrap();
                listener
                    .log_debug_message("cleaning up enabling automation mode")
                    .await
                    .unwrap();
                listener.did_begin_executing_test_plan().await.unwrap();
                pending().await
            }
        },
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    runner
        .abort_and_join_with(|| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn readiness_requires_http_200_and_boolean_true() {
    assert!(strict_ready(&ready().unwrap()));
    for status in [201, 204, 301, 404, 500, 503] {
        let mut response = ready().unwrap();
        response.status = status;
        assert!(!strict_ready(&response));
    }
    for body in [
        "{}",
        r#"{"ready":true}"#,
        r#"{"value":{"ready":false}}"#,
        r#"{"value":{"ready":"true"}}"#,
        r#"{"value":{"ready":1}}"#,
        r#"{"value":{"ready":null}}"#,
        r#"{"value":{"ready":true,"error":"unknown error"}}"#,
        r#"{"value":{"ready":true,"error":null}}"#,
        r#"{"error":"unknown error","value":{"ready":true}}"#,
        r#"{"value":{"ready":true}} trailing"#,
    ] {
        assert!(!strict_ready(&HttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
        }));
    }
}

#[tokio::test(start_paused = true)]
async fn started_plan_polls_until_strict_ready_and_keeps_task_owned() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let probes = AtomicUsize::new(0);
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || {
            let attempt = probes.fetch_add(1, Ordering::SeqCst);
            async move {
                match attempt {
                    0 => None,
                    1 => Some(HttpResponse {
                        status: 503,
                        body: ready().unwrap().body,
                    }),
                    _ => ready(),
                }
            }
        },
        deadline(),
    )
    .await
    .unwrap();
    assert_eq!(probes.load(Ordering::SeqCst), 3);
    assert!(!runner.is_finished());
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    runner
        .abort_and_join_with(|| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn task_exit_before_or_during_probe_prevents_ready() {
    for during_probe in [false, true] {
        for exit in [Ok(()), Err(RunnerError::RunnerFailed)] {
            let (release, released) = oneshot::channel();
            let mut release = Some(release);
            let result = OwnedWdaRunner::start_task(
                move |mut listener| async move {
                    listener.did_begin_executing_test_plan().await.unwrap();
                    if during_probe {
                        let _ = released.await;
                    }
                    exit
                },
                || {
                    if let Some(release) = release.take() {
                        let _ = release.send(());
                    }
                    async {
                        tokio::task::yield_now().await;
                        ready()
                    }
                },
                deadline(),
            )
            .await;
            assert_eq!(
                result.err(),
                Some(exit.err().unwrap_or(RunnerError::RunnerExited))
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn task_panic_is_a_fixed_error_and_is_not_joined_twice() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = OwnedWdaRunner::start_task(
        |mut listener| {
            let dropped = dropped.clone();
            async move {
                let _guard = OnDrop(dropped);
                listener.did_begin_executing_test_plan().await.unwrap();
                panic!("fixture task exit");
            }
        },
        || async { ready() },
        deadline(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::TaskFailed));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn finished_plan_prevents_ready_even_while_task_is_alive() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let (finish, finished) = oneshot::channel();
    let mut finish = Some(finish);
    let result = OwnedWdaRunner::start_task(
        |mut listener| {
            let dropped = dropped.clone();
            async move {
                let _guard = OnDrop(dropped);
                listener.did_begin_executing_test_plan().await.unwrap();
                finished.await.unwrap();
                listener.did_finish_executing_test_plan().await.unwrap();
                // Repeated begin callbacks cannot reopen a finished plan.
                listener.did_begin_executing_test_plan().await.unwrap();
                pending().await
            }
        },
        || {
            finish.take().unwrap().send(()).unwrap();
            async {
                tokio::task::yield_now().await;
                ready()
            }
        },
        deadline(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::RunnerExited));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn blocked_probe_is_cancelled_at_absolute_deadline_and_task_is_joined() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let probe_dropped = Arc::new(AtomicUsize::new(0));
    let expires = deadline();
    let result = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || {
            let probe_dropped = probe_dropped.clone();
            async move {
                let _guard = OnDrop(probe_dropped);
                pending().await
            }
        },
        expires,
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::StartupTimedOut));
    assert_eq!(Instant::now(), expires);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(probe_dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn readiness_at_deadline_is_too_late() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let expires = deadline();
    let result = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async {
            sleep_until(expires).await;
            ready()
        },
        expires,
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::StartupTimedOut));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn lookup_is_bounded_and_does_not_reset_startup_deadline() {
    for budget in [Duration::from_secs(2), Duration::from_secs(30)] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let result = lookup_before_deadline(
            async {
                let _guard = OnDrop(dropped.clone());
                pending::<Result<(), RunnerError>>().await
            },
            started + budget,
        )
        .await;
        assert_eq!(
            result,
            Err(if budget < LOOKUP_TIMEOUT {
                RunnerError::StartupTimedOut
            } else {
                RunnerError::LookupTimedOut
            })
        );
        assert_eq!(Instant::now() - started, budget.min(LOOKUP_TIMEOUT));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    let expires = Instant::now() + Duration::from_secs(5);
    lookup_before_deadline(
        async {
            sleep(Duration::from_secs(4)).await;
            Ok(())
        },
        expires,
    )
    .await
    .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { None },
        expires,
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::StartupTimedOut));
    assert_eq!(Instant::now(), expires);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn expired_startup_never_spawns_the_runner() {
    let result = OwnedWdaRunner::start_task(
        |_| {
            panic!("expired startup spawned a task");
            #[allow(unreachable_code)]
            async {
                Ok(())
            }
        },
        || async { ready() },
        Instant::now(),
    )
    .await;
    assert_eq!(result.err(), Some(RunnerError::StartupTimedOut));
}

#[tokio::test(start_paused = true)]
async fn cancelled_startup_aborts_the_task_without_detaching() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let task_dropped = dropped.clone();
    let (probing, probed) = oneshot::channel();
    let mut probing = Some(probing);
    let startup = tokio::spawn(OwnedWdaRunner::start_task(
        |listener| live(listener, task_dropped),
        move || {
            probing.take().unwrap().send(()).unwrap();
            pending()
        },
        deadline(),
    ));
    probed.await.unwrap();
    startup.abort();
    assert!(startup.await.err().unwrap().is_cancelled());
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_ready_handle_aborts_the_task() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    drop(runner);
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelling_abort_and_join_keeps_the_abort_guard() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    let mut shutdown = Box::pin(runner.abort_and_join_with(|| async { Ok(()) }));
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    drop(shutdown);
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_delayed_endpoint_refusal_after_host_join() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    let started = Instant::now();
    let mut probes = 0;
    runner
        .abort_and_join_with(|| {
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            probes += 1;
            let result = if probes < 3 {
                Err(RunnerError::EndpointBusy)
            } else {
                Ok(())
            };
            async move { result }
        })
        .await
        .unwrap();
    assert_eq!(probes, 3);
    assert_eq!(Instant::now() - started, 2 * POLL_INTERVAL);
}

#[tokio::test]
async fn shutdown_preserves_native_failure_checkpoint() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    let error = RunnerError::NativeProcess(NativeProcessStep::InventoryPath);
    assert_eq!(
        runner.abort_and_join_with(|| async { Err(error) }).await,
        Err(error)
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_refuses_open_unknown_or_unresponsive_endpoint() {
    for case in 0..4 {
        let dropped = Arc::new(AtomicUsize::new(0));
        let runner = OwnedWdaRunner::start_task(
            |listener| live(listener, dropped.clone()),
            || async { ready() },
            deadline(),
        )
        .await
        .unwrap();
        let started = Instant::now();
        let result = runner
            .abort_and_join_with(|| async move {
                match case {
                    0 => Err(RunnerError::EndpointBusy),
                    1 => Err(RunnerError::EndpointUnproven),
                    2 => Err(RunnerError::LookupFailed),
                    3 => pending().await,
                    _ => unreachable!(),
                }
            })
            .await;
        assert_eq!(result, Err(RunnerError::EndpointCleanupUnproven));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            Instant::now() - started,
            if case == 0 || case == 3 {
                SHUTDOWN_TIMEOUT
            } else {
                Duration::ZERO
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_checks_endpoint_even_when_host_task_already_finished() {
    for failed in [false, true] {
        for vacant in [false, true] {
            let (finish, finished) = oneshot::channel();
            let runner = OwnedWdaRunner::start_task(
                |mut listener| async move {
                    listener.did_begin_executing_test_plan().await.unwrap();
                    finished.await.unwrap();
                    if failed {
                        Err(RunnerError::RunnerFailed)
                    } else {
                        Ok(())
                    }
                },
                || async { ready() },
                deadline(),
            )
            .await
            .unwrap();
            finish.send(()).unwrap();
            tokio::task::yield_now().await;
            assert!(runner.is_finished());
            let mut probes = 0;
            let result = runner
                .abort_and_join_with(|| {
                    probes += 1;
                    async move {
                        if vacant {
                            Ok(())
                        } else {
                            Err(RunnerError::EndpointUnproven)
                        }
                    }
                })
                .await;
            assert_eq!(probes, 1);
            assert_eq!(
                result,
                if !vacant {
                    Err(RunnerError::EndpointCleanupUnproven)
                } else if failed {
                    Err(RunnerError::RunnerFailed)
                } else {
                    Ok(())
                }
            );
        }
    }
}

#[tokio::test]
async fn shutdown_without_pinned_endpoint_never_claims_cleanup() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let runner = OwnedWdaRunner::start_task(
        |listener| live(listener, dropped.clone()),
        || async { ready() },
        deadline(),
    )
    .await
    .unwrap();
    assert_eq!(
        runner.abort_and_join().await,
        Err(RunnerError::RunnerStopUnproven)
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn profile_expiry_refuses_only_when_effective_expiry_has_passed() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let hour = Duration::from_secs(3600);
    let live = ProfileExpiry {
        profile: now + 100 * hour,
        certificate: Some(now + 10 * hour),
    };
    assert_eq!(classify_profile(Some(live), now), Ok(live));
    let certificate_expired = ProfileExpiry {
        certificate: Some(now - hour),
        ..live
    };
    assert_eq!(
        classify_profile(Some(certificate_expired), now),
        Err(RunnerError::ProfileExpired)
    );
    let profile_expired = ProfileExpiry {
        profile: now,
        certificate: None,
    };
    assert_eq!(
        classify_profile(Some(profile_expired), now),
        Err(RunnerError::ProfileExpired)
    );
    assert_eq!(
        classify_profile(None, now),
        Err(RunnerError::ProfileUnavailable)
    );
    assert_eq!(RunnerError::ProfileExpired.to_string(), "ProfileExpired");
}
