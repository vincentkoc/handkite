use handkite_core::controller::{AdapterError, Controller, DeviceAdapter, RawObservation};
use handkite_core::protocol::{Action, Observation, TransportReceipt};
use handkite_host::host_config;
use handkite_host::wda_adapter::{CaptureTimings, WdaAdapter};
use handkite_host::wire::{Reply, WireObservation};
use serde::Serialize;
use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("invalid_arguments");
    };
    if flag != "--config" {
        return Err("invalid_arguments");
    }
    let binding = host_config::load(Path::new(path)).map_err(|_| "configuration_invalid")?;
    let capture = RefCell::new(CaptureTimings::default());
    let adapter = MeasuredAdapter {
        inner: WdaAdapter::new(binding.usb_id.clone()).map_err(|_| "adapter_unavailable")?,
        capture: &capture,
    };
    let key = binding.key.clone();
    let controller = Controller::new(binding, adapter, 5000, 5000);
    let (result, observation) = timed(&Instant::now, |_| 0, || controller.observe(&key));
    let mut report = Report {
        outcome: if result.is_ok() {
            "complete"
        } else {
            "observation_failed"
        },
        capture: capture.take(),
        observation,
        wire_encode: None,
        wire_json: None,
    };
    if let Ok(observation) = result {
        report.measure_serialization(observation, &Instant::now);
    }
    // Only this fixed report is written. Captured XML, pixels and identities never enter it.
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|_| "report_failed")?
    );
    if report.outcome == "complete" {
        Ok(())
    } else {
        Err(report.outcome)
    }
}

struct MeasuredAdapter<'a> {
    inner: WdaAdapter,
    capture: &'a RefCell<CaptureTimings>,
}

impl DeviceAdapter for MeasuredAdapter<'_> {
    fn identities(
        &mut self,
    ) -> Result<
        (
            handkite_core::protocol::HardwareId,
            handkite_core::protocol::HardwareId,
        ),
        AdapterError,
    > {
        self.inner.identities()
    }
    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        let (result, capture) = self.inner.observe_with_timings();
        self.capture.replace(capture);
        result
    }
    fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
        Err(AdapterError)
    }
}

#[derive(Serialize)]
struct Timing {
    elapsed_us: u64,
    bytes: Option<usize>,
    ok: bool,
}

fn timed<T, E>(
    clock: &impl Fn() -> Instant,
    bytes: impl FnOnce(&T) -> usize,
    work: impl FnOnce() -> Result<T, E>,
) -> (Result<T, E>, Timing) {
    let start = clock();
    let result = work();
    let end = clock();
    let timing = Timing {
        elapsed_us: end
            .saturating_duration_since(start)
            .as_micros()
            .min(u64::MAX as u128) as u64,
        bytes: result.as_ref().ok().map(bytes),
        ok: result.is_ok(),
    };
    (result, timing)
}

#[derive(Serialize)]
struct Report {
    outcome: &'static str,
    capture: CaptureTimings,
    observation: Timing,
    wire_encode: Option<Timing>,
    wire_json: Option<Timing>,
}

impl Report {
    fn measure_serialization(&mut self, observation: Observation, clock: &impl Fn() -> Instant) {
        let (wire, encoding) = timed(
            clock,
            |wire: &WireObservation| wire.screenshot_base64.len(),
            || Ok::<_, ()>(WireObservation::from(observation)),
        );
        self.wire_encode = Some(encoding);
        let reply = Reply::Observed {
            observation: wire.expect("infallible conversion"),
        };
        // Match the service serializer and its limit; retain no content in the report.
        let (bytes, mut json) = timed(clock, Vec::len, || serde_json::to_vec(&reply));
        if bytes.as_ref().is_ok_and(|bytes| bytes.len() > MAX_RESPONSE) {
            json.ok = false;
        }
        if !json.ok {
            self.outcome = "serialization_failed";
        }
        self.wire_json = Some(json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::protocol::{
        DeviceKey, DeviceState, ImageFormat, LockState, ObservationWindow, Revision, ScreenImage,
    };
    use std::cell::Cell;
    use std::time::Duration;

    fn observation() -> Observation {
        Observation {
            device: DeviceKey::new("private-device").unwrap(),
            revision: Revision(1),
            window: ObservationWindow {
                started_unix_ms: 1,
                finished_unix_ms: 2,
            },
            state: DeviceState {
                lock: LockState::Locked,
                wda_ready: true,
                foreground_app: Some("private-app".into()),
            },
            accessibility_tree: "private-tree".into(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: b"private-pixels".to_vec(),
            },
        }
    }

    #[test]
    fn exact_wire_serialization_is_timed_without_exposing_content() {
        let base = Instant::now();
        let ticks = Cell::new(0);
        let clock = || {
            let n = ticks.get();
            ticks.set(n + 11);
            base + Duration::from_micros(n)
        };
        let expected = serde_json::to_vec(&Reply::Observed {
            observation: observation().into(),
        })
        .unwrap();
        let mut report = Report {
            outcome: "complete",
            capture: CaptureTimings::default(),
            observation: Timing {
                elapsed_us: 1,
                bytes: None,
                ok: true,
            },
            wire_encode: None,
            wire_json: None,
        };
        report.measure_serialization(observation(), &clock);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["wire_encode"]["elapsed_us"], 11);
        assert_eq!(value["wire_json"]["elapsed_us"], 11);
        assert_eq!(value["wire_json"]["bytes"], expected.len());
        assert_eq!(value["wire_encode"]["bytes"], 20);
        assert!(!value.to_string().contains("private"));
        assert!(!value.to_string().contains("cHJpdmF0ZS1waXhlbHM="));
    }

    #[test]
    fn oversized_wire_fails_without_partial_content_or_retry() {
        let mut raw = observation();
        raw.accessibility_tree = "x".repeat(MAX_RESPONSE);
        let mut report = Report {
            outcome: "complete",
            capture: CaptureTimings::default(),
            observation: Timing {
                elapsed_us: 1,
                bytes: None,
                ok: true,
            },
            wire_encode: None,
            wire_json: None,
        };
        report.measure_serialization(raw, &Instant::now);
        assert_eq!(report.outcome, "serialization_failed");
        let json = report.wire_json.unwrap();
        assert!(!json.ok);
        assert!(json.bytes.unwrap() > MAX_RESPONSE);
    }

    #[test]
    fn invalid_arguments_do_not_open_config_or_capture() {
        for args in [
            vec![],
            vec!["--config".into()],
            vec!["--other".into(), "private".into()],
        ] {
            assert_eq!(command(&args), Err("invalid_arguments"));
        }
    }
}
