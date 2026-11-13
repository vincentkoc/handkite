use crate::wda_http::HttpResponse;
use crate::wda_transport::WdaError;
use serde::Serialize;
use std::future::Future;
use std::time::Instant;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    OwnershipBefore,
    ScreenshotRequest,
    SourceRequest,
    AppRequest,
    LockRequest,
    ScreenshotJson,
    SourceJson,
    AppJson,
    LockJson,
    ImageDecode,
    PayloadValidation,
    OwnershipAfter,
    CaptureTotal,
}

/// Content-free measurements from one capture. Missing stages were not reached.
#[derive(Default, Serialize)]
pub struct CaptureTimings {
    stages: Vec<Sample>,
}

#[derive(Serialize)]
pub(super) struct Sample {
    stage: Stage,
    elapsed_us: u64,
    bytes: Option<usize>,
    ok: bool,
}

impl CaptureTimings {
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
                let mut expected = vec![
                    "/screenshot",
                    SCREEN_SOURCE,
                    "/wda/activeAppInfo",
                    "/wda/locked",
                ];
                expected.sort();
                assert_eq!(calls, expected);
                assert_eq!(clock().duration_since(anchor), Duration::from_millis(4));
                if let Ok((raw, finished, lock)) = result {
                    assert_eq!(raw.accessibility_tree, "private-tree");
                    assert_eq!(raw.window.started_unix_ms, 17);
                    assert_eq!(raw.window.finished_unix_ms, 21);
                    assert_eq!(finished.duration_since(anchor), Duration::from_millis(4));
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
                assert_eq!(stages[1]["elapsed_us"], 1000);
                assert_eq!(stages[2]["elapsed_us"], 2000);
                assert_eq!(stages[3]["elapsed_us"], 3000);
                assert_eq!(stages[0]["ok"], case != 1);
                assert_eq!(stages[3]["ok"], case != 7);
                if !matches!(case, 0 | 1 | 6) {
                    assert!(stages.iter().any(|stage| stage["ok"] == false));
                }
            }
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
