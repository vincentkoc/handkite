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
    runner.abort_and_join().await.unwrap();
    assert!(liveness.is_finished());
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
            OwnedWdaRunner::start(usb.clone(), bundle, Duration::from_secs(1))
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
            OwnedWdaRunner::start(usb.clone(), "com.example.xctrunner", timeout)
                .await
                .err(),
            Some(RunnerError::InvalidTimeout)
        );
    }
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
    assert_eq!(result.err(), Some(RunnerError::StartupTimedOut));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
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
    runner.abort_and_join().await.unwrap();
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
    let mut shutdown = Box::pin(runner.abort_and_join());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    drop(shutdown);
    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
