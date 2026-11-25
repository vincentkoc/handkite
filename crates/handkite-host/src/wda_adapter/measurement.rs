use crate::wda_http::HttpResponse;
use crate::wda_transport::WdaError;
use serde::Serialize;
use std::future::Future;
use std::time::Instant;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    OwnershipBefore,
    ScreenshotBeforeRequest,
    ScreenshotBeforeJson,
    AppBeforeRequest,
    AppBeforeJson,
    LockBeforeRequest,
    LockBeforeJson,
    ScreenshotRequest,
    SourceRequest,
    AppRequest,
    LockRequest,
    ScreenshotJson,
    OutlineSourceRequest,
    OutlineCost,
    SourceJson,
    AppJson,
    LockJson,
    ImageDecode,
    CaptureCoherence,
    PayloadValidation,
    OwnershipAfter,
    CaptureTotal,
}

/// Content-free measurements from one capture. Missing stages were not reached.
#[derive(Default, Serialize)]
pub struct CaptureTimings {
    stages: Vec<Sample>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reused_tree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    outline_nodes: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct Sample {
    stage: Stage,
    elapsed_us: u64,
    bytes: Option<usize>,
    ok: bool,
}

impl CaptureTimings {
    pub(super) fn outline_nodes(&mut self, nodes: usize) {
        self.outline_nodes = Some(nodes);
    }
    pub(super) fn mark_reused_tree(&mut self) {
        self.reused_tree = true;
    }
    pub(super) fn append(&mut self, samples: impl IntoIterator<Item = Option<Sample>>) {
        self.stages.extend(samples.into_iter().flatten());
    }

    pub(super) fn finish(&mut self, start: Instant, ok: bool) {
        self.stages
            .push(sample(Stage::CaptureTotal, start, Instant::now(), None, ok));
    }
}

fn sample(stage: Stage, start: Instant, end: Instant, bytes: Option<usize>, ok: bool) -> Sample {
    Sample {
        stage,
        elapsed_us: end
            .saturating_duration_since(start)
            .as_micros()
            .min(u64::MAX as u128) as u64,
        bytes,
        ok,
    }
}

pub(super) fn measure<T, E>(
    timings: Option<&mut CaptureTimings>,
    stage: Stage,
    clock: &impl Fn() -> Instant,
    bytes: impl FnOnce(&T) -> usize,
    work: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let start = timings.as_ref().map(|_| clock());
    let result = work();
    if let (Some(timings), Some(start)) = (timings, start) {
        let end = clock();
        let count = result.as_ref().ok().map(bytes);
        timings
            .stages
            .push(sample(stage, start, end, count, result.is_ok()));
    }
    result
}

pub(super) async fn request(
    enabled: bool,
    stage: Stage,
    clock: &impl Fn() -> Instant,
    work: impl Future<Output = Result<HttpResponse, WdaError>>,
) -> (Result<HttpResponse, WdaError>, Option<Sample>) {
    let start = enabled.then(clock);
    let result = work.await;
    let timing = start.map(|start| {
        sample(
            stage,
            start,
            clock(),
            result.as_ref().ok().map(|response| response.body.len()),
            result.as_ref().is_ok_and(|response| response.status == 200),
        )
    });
    (result, timing)
}

#[cfg(test)]
mod tests {
    use super::super::{SCREEN_SOURCE, WdaAdapter};
    use super::*;
    use crate::wda_action::WdaRequest;
    use base64::Engine;
    use handkite_core::protocol::HardwareId;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    #[test]
    fn enabled_clock_records_failure_without_content() {
        let base = Instant::now();
        let ticks = Cell::new(0);
        let clock = || {
            let tick = ticks.get();
            ticks.set(tick + 7);
            base + Duration::from_micros(tick)
        };
        let mut timings = CaptureTimings::default();
        let result = measure(
            Some(&mut timings),
            Stage::SourceJson,
            &clock,
            |s: &String| s.len(),
            || Err::<String, _>("private fixture error"),
        );
        assert!(result.is_err());
        assert_eq!(
            serde_json::to_value(&timings).unwrap(),
            serde_json::json!({
                "stages": [{"stage":"source_json", "elapsed_us":7, "bytes":null, "ok":false}]
            })
        );
    }

    #[test]
    fn disabled_measurement_does_not_read_clock_or_count_bytes() {
        assert_eq!(
            measure(
                None,
                Stage::SourceJson,
                &|| panic!("clock"),
                |_: &u8| panic!("bytes"),
                || Ok::<_, ()>(3)
            ),
            Ok(3)
        );
    }

    struct Reads {
        case: u8,
        calls: RefCell<Vec<String>>,
    }

    struct ChangingScreen {
        screenshots: Cell<usize>,
        apps: Cell<usize>,
        locks: Cell<usize>,
        change: u8,
    }

    impl ChangingScreen {
        fn new(change: u8) -> Self {
            Self {
                screenshots: Cell::new(0),
                apps: Cell::new(0),
                locks: Cell::new(0),
                change,
            }
        }
    }

    impl WdaRequest for ChangingScreen {
        async fn request(
            &self,
            _: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            if self.change == 4 {
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
            let value = match path {
                "/screenshot" => {
                    let call = self.screenshots.get();
                    self.screenshots.set(call + 1);
                    let drifts = matches!(self.change, 1 | 5..=12);
                    if (self.change == 9 && call == 0) || (self.change == 10 && call > 0) {
                        return Ok(HttpResponse {
                            status: 200,
                            body: br#"{"value":"not-a-png"}"#.to_vec(),
                        });
                    }
                    let pixels: &[u8] =
                        if call == 0 || !drifts || (self.change == 12 && call % 2 == 0) {
                            b"\x89PNG\r\n\x1a\nfirst-picker"
                        } else {
                            b"\x89PNG\r\n\x1a\nsecond-picker"
                        };
                    json!(base64::engine::general_purpose::STANDARD.encode(pixels))
                }
                SCREEN_SOURCE if self.change == 8 => json!(""),
                SCREEN_SOURCE if self.change == 11 && self.screenshots.get() > 2 => {
                    json!("<Application><Cell label=\"second-picker\"/></Application>")
                }
                SCREEN_SOURCE => json!("<Application><Cell label=\"first-picker\"/></Application>"),
                "/wda/activeAppInfo" => {
                    let call = self.apps.get();
                    self.apps.set(call + 1);
                    json!({"bundleId": if matches!(self.change, 2 | 5) && call > 0 { "other.app" } else { "fixture.app" }})
                }
                "/wda/locked" => {
                    let call = self.locks.get();
                    self.locks.set(call + 1);
                    if self.change == 7 {
                        json!(null)
                    } else {
                        json!(matches!(self.change, 3 | 6) && call > 0)
                    }
                }
                _ => panic!("unexpected request"),
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"value":value})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn changing_picker_cannot_publish_a_paired_observation() {
        for change in (1..=3).chain(5..=10) {
            let reads = ChangingScreen::new(change);
            let result = WdaAdapter::capture_using(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                SCREEN_SOURCE.into(),
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                None,
            )
            .await;
            assert_eq!(
                result.err(),
                Some(if change == 1 {
                    super::super::CaptureFailure::ImageDrift
                } else {
                    super::super::CaptureFailure::Adapter
                }),
                "only valid image drift, without app/lock/schema errors, is recoverable: {change}"
            );
        }
    }

    #[test]
    fn lock_retry_replaces_both_halves_and_keeps_failure_boundaries() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for change in [2, 3, 5, 6, 7, 8, 9, 10, 11, 12] {
            let reads = ChangingScreen::new(change);
            let result = super::super::retry_lock_capture(&Instant::now, |deadline| {
                runtime.block_on(WdaAdapter::capture_using(
                    &reads,
                    &HardwareId::new("fixture-device").unwrap(),
                    SCREEN_SOURCE.into(),
                    17,
                    &Instant::now,
                    &|| Ok(21),
                    None,
                    Some(deadline),
                ))
            });
            assert_eq!(result.is_ok(), change == 11, "change {change}");
            assert_eq!(reads.screenshots.get(), if change >= 11 { 4 } else { 2 });
            if let Ok((raw, _, _)) = result {
                assert!(raw.accessibility_tree.contains("second-picker"));
                assert!(raw.screenshot.bytes.ends_with(b"second-picker"));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coherence_reads_share_one_total_capture_deadline() {
        let start = tokio::time::Instant::now();
        let reads = ChangingScreen::new(4);
        assert!(
            WdaAdapter::capture_using(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                SCREEN_SOURCE.into(),
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                None,
            )
            .await
            .is_err()
        );
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn capture_honors_the_remaining_lock_read_budget() {
        let start = tokio::time::Instant::now();
        let reads = ChangingScreen::new(4);
        let deadline = (start + Duration::from_secs(5)).into_std();
        assert!(
            WdaAdapter::capture_using(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                SCREEN_SOURCE.into(),
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                Some(deadline),
            )
            .await
            .is_err()
        );
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(5));
    }

    impl WdaRequest for Reads {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            budget: Duration,
        ) -> Result<HttpResponse, WdaError> {
            assert_eq!(method, "GET");
            assert!(body.is_empty());
            self.calls.borrow_mut().push(path.into());
            let (delay, mut value) = match path {
                "/screenshot" => {
                    assert_eq!(budget, Duration::from_secs(20));
                    (
                        4,
                        json!(
                            base64::engine::general_purpose::STANDARD
                                .encode(b"\x89PNG\r\n\x1a\nprivate-pixels")
                        ),
                    )
                }
                SCREEN_SOURCE => {
                    assert_eq!(budget, Duration::from_secs(20));
                    (
                        if self.case == 13 {
                            12_000
                        } else if self.case >= 14 {
                            6_200
                        } else {
                            1
                        },
                        json!("private-tree"),
                    )
                }
                super::super::OUTLINE_SOURCE => (
                    1,
                    json!(if self.case >= 14 {
                        "<XCUIElementTypeApplication><XCUIElementTypeWebView/></XCUIElementTypeApplication>"
                    } else {
                        "<XCUIElementTypeApplication/>"
                    }),
                ),
                "/wda/activeAppInfo" => (2, json!({"bundleId":"private-app"})),
                "/wda/locked" => (3, json!(false)),
                _ => panic!("unexpected request"),
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if self.case == 1 && path == "/screenshot" {
                return Err(WdaError::NotConnected);
            }
            if self.case == 2 && path == SCREEN_SOURCE {
                return Ok(HttpResponse {
                    status: 200,
                    body: b"invalid-private-json".to_vec(),
                });
            }
            if self.case == 3 && path == "/screenshot" {
                value = json!(null);
            }
            if self.case == 4 && path == SCREEN_SOURCE {
                value = json!("");
            }
            if self.case == 5 && path == "/wda/activeAppInfo" {
                value = json!({});
            }
            if self.case == 6 && path == "/wda/locked" {
                value = json!(null);
            }
            if (8..=13).contains(&self.case) && path == "/wda/activeAppInfo" {
                value = if self.case == 12 {
                    json!({})
                } else {
                    json!({"bundleId":"com.apple.springboard"})
                };
            }
            if self.case == 9 && path == "/wda/locked" {
                value = json!(true);
            }
            if self.case == 10 && path == "/wda/locked" {
                value = json!(null);
            }
            if self.case == 11 && path == "/screenshot" {
                value = json!("invalid-image");
            }
            if self.case >= 14 && path == "/wda/activeAppInfo" {
                value = json!({"bundleId":"com.apple.mobilesafari"});
            }
            if self.case == 15 && path == "/wda/locked" {
                value = json!(true);
            }
            if self.case == 16 && path == "/wda/locked" {
                value = json!(null);
            }
            if self.case == 17
                && path == "/screenshot"
                && self
                    .calls
                    .borrow()
                    .iter()
                    .filter(|p| p.as_str() == "/screenshot")
                    .count()
                    == 2
            {
                value = json!(
                    base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nchanged")
                );
            }
            Ok(HttpResponse {
                status: if self.case == 7 && path == "/wda/locked" {
                    500
                } else {
                    200
                },
                body: serde_json::to_vec(&json!({"value":value})).unwrap(),
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn outline_cost_profile_keeps_full_and_locked_paths_unchanged() {
        use super::super::{CaptureFailure, CaptureSource, OUTLINE_SOURCE};
        for (case, source, light) in [
            (14, CaptureSource::Outline(SCREEN_SOURCE), true),
            (14, CaptureSource::FullTree(SCREEN_SOURCE), false),
            (14, CaptureSource::NavigateHome(SCREEN_SOURCE), false),
            (0, CaptureSource::Outline(SCREEN_SOURCE), false),
            (15, CaptureSource::Outline(SCREEN_SOURCE), false),
            (16, CaptureSource::Outline(SCREEN_SOURCE), false),
            (17, CaptureSource::Outline(SCREEN_SOURCE), true),
        ] {
            let reads = Reads {
                case,
                calls: RefCell::new(Vec::new()),
            };
            let result = WdaAdapter::capture_using_scoped(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                source,
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                None,
            )
            .await;
            if case == 17 {
                assert_eq!(result.err(), Some(CaptureFailure::ImageDrift));
            } else {
                assert_eq!(result.unwrap().1, light);
            }
            let calls = reads.calls.borrow();
            let profiled = matches!(source, CaptureSource::Outline(_)) && !matches!(case, 15 | 16);
            assert_eq!(calls.len(), 7 + usize::from(profiled && !light));
            assert_eq!(
                calls
                    .iter()
                    .filter(|p| p.as_str() == OUTLINE_SOURCE)
                    .count(),
                usize::from(profiled)
            );
            assert_eq!(
                calls.iter().filter(|p| p.as_str() == SCREEN_SOURCE).count(),
                usize::from(!light)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cold_home_routes_before_xml_only_for_passive_unlocked_capture() {
        use super::super::{CaptureFailure, CaptureSource};
        for (case, passive) in [
            (8, true),
            (8, false),
            (13, true),
            (13, false),
            (0, true),
            (9, true),
            (10, true),
            (11, true),
            (12, true),
        ] {
            let reads = Reads {
                case,
                calls: RefCell::new(Vec::new()),
            };
            let started = tokio::time::Instant::now();
            let source = if passive {
                CaptureSource::NavigateHome(SCREEN_SOURCE)
            } else {
                CaptureSource::FullTree(SCREEN_SOURCE)
            };
            let result = WdaAdapter::capture_using(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                source,
                17,
                &Instant::now,
                &|| Ok(21),
                None,
                None,
            )
            .await;
            let routed = result.as_ref().err() == Some(&CaptureFailure::HomeNavigation);
            assert_eq!(
                routed,
                matches!(case, 8 | 13) && passive,
                "case {case}, passive {passive}"
            );
            if case == 13 {
                if passive {
                    assert!(started.elapsed() < Duration::from_secs(1));
                } else {
                    assert!(started.elapsed() >= Duration::from_secs(12));
                }
            }
            let calls = reads.calls.borrow();
            if routed {
                assert!(!calls.iter().any(|path| path == SCREEN_SOURCE));
                assert_eq!(
                    calls
                        .iter()
                        .filter(|path| path.as_str() == "/screenshot")
                        .count(),
                    1
                );
            } else if matches!(case, 0 | 8 | 9 | 10 | 13) {
                assert!(calls.iter().any(|path| path == SCREEN_SOURCE));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn measured_capture_preserves_requests_windows_and_failure_behavior() {
        for case in 0..8 {
            for enabled in [false, true] {
                let reads = Reads {
                    case,
                    calls: RefCell::new(Vec::new()),
                };
                let anchor = Instant::now();
                let virtual_start = tokio::time::Instant::now();
                let clock_reads = Cell::new(0);
                let clock = || {
                    clock_reads.set(clock_reads.get() + 1);
                    anchor + tokio::time::Instant::now().duration_since(virtual_start)
                };
                let mut timings = CaptureTimings::default();
                let result = WdaAdapter::capture_using(
                    &reads,
                    &HardwareId::new("fixture-device").unwrap(),
                    SCREEN_SOURCE.into(),
                    17,
                    &clock,
                    &|| Ok(21),
                    enabled.then_some(&mut timings),
                    None,
                )
                .await;
                assert_eq!(result.is_ok(), matches!(case, 0 | 6), "case {case}");
                if !enabled {
                    assert_eq!(clock_reads.get(), u32::from(result.is_ok()));
                }
                let mut calls = reads.calls.into_inner();
                calls.sort();
                let early_failure = matches!(case, 1 | 3 | 5 | 7);
                let mut expected = if early_failure {
                    vec!["/screenshot", "/wda/activeAppInfo", "/wda/locked"]
                } else {
                    vec![
                        "/screenshot",
                        "/screenshot",
                        SCREEN_SOURCE.into(),
                        "/wda/activeAppInfo",
                        "/wda/locked",
                        "/wda/activeAppInfo",
                        "/wda/locked",
                    ]
                };
                expected.sort();
                assert_eq!(calls, expected);
                let elapsed = Duration::from_millis(if early_failure { 4 } else { 9 });
                assert_eq!(clock().duration_since(anchor), elapsed);
                if let Ok((raw, finished, lock)) = result {
                    assert_eq!(raw.accessibility_tree, "private-tree");
                    assert_eq!(raw.window.started_unix_ms, 17);
                    assert_eq!(raw.window.finished_unix_ms, 21);
                    assert_eq!(finished.duration_since(anchor), elapsed);
                    assert_eq!(lock, if case == 6 { None } else { Some(false) });
                }
                let report = serde_json::to_value(&timings).unwrap();
                assert!(!report.to_string().contains("private"));
                let stages = report["stages"].as_array().unwrap();
                if !enabled {
                    assert!(stages.is_empty());
                    continue;
                }
                assert_eq!(stages[0]["elapsed_us"], 4000);
                assert_eq!(stages[0]["ok"], case != 1);
                if !early_failure {
                    for (name, elapsed) in [
                        ("source_request", 1000),
                        ("app_request", 2000),
                        ("lock_request", 3000),
                    ] {
                        let stage = stages.iter().find(|stage| stage["stage"] == name).unwrap();
                        assert_eq!(stage["elapsed_us"], elapsed);
                        assert_eq!(stage["ok"], !(name == "lock_request" && case == 7));
                    }
                }
                if !matches!(case, 0 | 1 | 6) {
                    assert!(stages.iter().any(|stage| stage["ok"] == false));
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn frame_probe_never_requests_or_fabricates_source() {
        for case in [0, 1, 3, 5, 6, 7] {
            let reads = Reads {
                case,
                calls: RefCell::new(Vec::new()),
            };
            let mut timings = CaptureTimings::default();
            let result =
                WdaAdapter::capture_payload(&reads, None, &Instant::now, Some(&mut timings), None)
                    .await;
            assert_eq!(result.is_ok(), matches!(case, 0 | 6));
            if let Ok(frame) = result {
                assert!(frame.tree.is_none());
            }
            let mut calls = reads.calls.into_inner();
            calls.sort();
            assert_eq!(calls, ["/screenshot", "/wda/activeAppInfo", "/wda/locked"]);
            let report = serde_json::to_value(&timings).unwrap();
            assert!(!report.to_string().contains("private"));
            assert!(
                report["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|stage| !stage["stage"].as_str().unwrap().starts_with("source"))
            );
        }
    }

    #[test]
    #[ignore = "local instrumentation microbenchmark; no device work"]
    fn instrumentation_overhead() {
        const ITERATIONS: u32 = 10_000;
        for enabled in [false, true] {
            let start = Instant::now();
            for _ in 0..ITERATIONS {
                let mut timings = CaptureTimings::default();
                for _ in 0..13 {
                    let result = measure(
                        enabled.then_some(&mut timings),
                        Stage::SourceJson,
                        &Instant::now,
                        |_| 16,
                        || Ok::<_, ()>(std::hint::black_box(7)),
                    );
                    std::hint::black_box(result).unwrap();
                }
                std::hint::black_box(timings);
            }
            println!(
                "capture_measurement enabled={enabled} ns_per_13_stages={}",
                start.elapsed().as_nanos() / u128::from(ITERATIONS)
            );
        }
    }
}

#[cfg(test)]
mod lock_recovery_tests {
    use super::super::{CaptureFailure, retry_lock_capture};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[test]
    fn lock_discards_drifted_capture_and_uses_one_fresh_pair() {
        let start = Instant::now();
        let mut calls = 0;
        let mut deadlines = Vec::new();
        let result = retry_lock_capture(&|| start, |deadline| {
            deadlines.push(deadline);
            calls += 1;
            if calls == 1 {
                Err(CaptureFailure::ImageDrift)
            } else {
                Ok("fresh pair")
            }
        });
        assert_eq!(result, Ok("fresh pair"));
        assert_eq!(calls, 2);
        assert_eq!(deadlines, vec![start + Duration::from_secs(20); 2]);
    }

    #[test]
    fn lock_never_retries_non_drift_failure_or_a_second_drift() {
        for first in [CaptureFailure::Adapter, CaptureFailure::ImageDrift] {
            let mut calls = 0;
            let result = retry_lock_capture(&Instant::now, |_| {
                calls += 1;
                Err::<(), _>(first)
            });
            assert_eq!(result, Err(first));
            assert_eq!(
                calls,
                if first == CaptureFailure::Adapter {
                    1
                } else {
                    2
                }
            );
        }
    }

    #[test]
    fn lock_retry_does_not_renew_the_capture_deadline() {
        let start = Instant::now();
        let expired = Cell::new(false);
        let mut calls = 0;
        let result = retry_lock_capture(
            &|| {
                if expired.get() {
                    start + Duration::from_secs(20)
                } else {
                    start
                }
            },
            |_| {
                calls += 1;
                expired.set(true);
                Err::<(), _>(CaptureFailure::ImageDrift)
            },
        );
        assert_eq!(result, Err(CaptureFailure::Adapter));
        assert_eq!(calls, 1);
    }
}

#[cfg(test)]
mod observe_recovery_tests {
    use super::super::{CaptureFailure, retry_observe_capture};
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[test]
    fn successful_observe_never_replays_its_capture() {
        let mut calls = 0;
        let result = retry_observe_capture(&Instant::now, |_| {
            calls += 1;
            Ok("first coherent pair")
        });
        assert_eq!(result, Ok("first coherent pair"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn home_route_marker_is_never_retried_as_image_drift() {
        let mut calls = 0;
        let result = retry_observe_capture(&Instant::now, |_| {
            calls += 1;
            Err::<(), _>(CaptureFailure::HomeNavigation)
        });
        assert_eq!(result, Err(CaptureFailure::HomeNavigation));
        assert_eq!(calls, 1);
    }

    #[test]
    fn slow_drift_keeps_the_existing_fallback_without_another_capture() {
        let start = Instant::now();
        let now = Cell::new(start);
        let mut calls = 0;
        let result = retry_observe_capture(&|| now.get(), |_| {
            calls += 1;
            now.set(start + Duration::from_secs(1));
            Err::<(), _>(CaptureFailure::ImageDrift)
        });
        assert_eq!(result, Err(CaptureFailure::ImageDrift));
        assert_eq!(calls, 1);
    }

    #[test]
    fn transient_observe_drift_uses_only_a_complete_replacement_pair() {
        let start = Instant::now();
        let mut calls = 0;
        let mut deadlines = vec![];
        let result = retry_observe_capture(&|| start, |deadline| {
            deadlines.push(deadline);
            calls += 1;
            if calls == 1 {
                Err(CaptureFailure::ImageDrift)
            } else {
                Ok(("new tree", "new image"))
            }
        });
        assert_eq!(result, Ok(("new tree", "new image")));
        assert_eq!(deadlines, [start + Duration::from_secs(20); 2]);
    }

    #[test]
    fn observe_preserves_second_failure_and_never_tries_a_third_pair() {
        for second in [CaptureFailure::Adapter, CaptureFailure::ImageDrift] {
            let mut calls = 0;
            let result = retry_observe_capture(&Instant::now, |_| {
                calls += 1;
                Err::<(), _>(if calls == 1 {
                    CaptureFailure::ImageDrift
                } else {
                    second
                })
            });
            assert_eq!(result, Err(second));
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn replacement_observe_cannot_renew_the_original_deadline() {
        let start = Instant::now();
        let now = Cell::new(start);
        let mut calls = 0;
        let result = retry_observe_capture(&|| now.get(), |_| {
            calls += 1;
            if calls == 1 {
                Err(CaptureFailure::ImageDrift)
            } else {
                now.set(start + Duration::from_secs(20));
                Ok("late pair")
            }
        });
        assert_eq!(result, Err(CaptureFailure::Adapter));
        assert_eq!(calls, 2);
    }
}
