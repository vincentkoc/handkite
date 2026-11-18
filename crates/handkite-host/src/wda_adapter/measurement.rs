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
}

#[derive(Serialize)]
pub(super) struct Sample {
    stage: Stage,
    elapsed_us: u64,
    bytes: Option<usize>,
    ok: bool,
}

impl CaptureTimings {
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
                    let drifts = matches!(self.change, 1 | 5..=10);
                    if (self.change == 9 && call == 0) || (self.change == 10 && call > 0) {
                        return Ok(HttpResponse {
                            status: 200,
                            body: br#"{"value":"not-a-png"}"#.to_vec(),
                        });
                    }
                    let pixels: &[u8] = if call == 0 || !drifts {
                        b"\x89PNG\r\n\x1a\nfirst-picker"
                    } else {
                        b"\x89PNG\r\n\x1a\nsecond-picker"
                    };
                    json!(base64::engine::general_purpose::STANDARD.encode(pixels))
                }
                SCREEN_SOURCE if self.change == 8 => json!(""),
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
                SCREEN_SOURCE,
                17,
                &Instant::now,
                &|| Ok(21),
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

    #[tokio::test(start_paused = true)]
    async fn coherence_reads_share_one_total_capture_deadline() {
        let start = tokio::time::Instant::now();
        let reads = ChangingScreen::new(4);
        assert!(
            WdaAdapter::capture_using(
                &reads,
                &HardwareId::new("fixture-device").unwrap(),
                SCREEN_SOURCE,
                17,
                &Instant::now,
                &|| Ok(21),
                None,
            )
            .await
            .is_err()
        );
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(20));
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
                    (1, json!("private-tree"))
                }
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
                    SCREEN_SOURCE,
                    17,
                    &clock,
                    &|| Ok(21),
                    enabled.then_some(&mut timings),
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
                        SCREEN_SOURCE,
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
                WdaAdapter::capture_payload(&reads, None, &Instant::now, Some(&mut timings)).await;
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
