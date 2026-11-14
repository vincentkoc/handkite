//! Opt-in, content-free timing for the existing service and MCP paths.
use crate::wda_adapter::CaptureTimings;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();
static EPOCH: OnceLock<Instant> = OnceLock::new();
static OPERATION: AtomicU64 = AtomicU64::new(0);
static SINK: OnceLock<Option<SyncSender<Envelope>>> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize)]
struct Envelope {
    sequence: u64,
    #[serde(flatten)]
    record: Record,
}

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("HANDKITE_TIMINGS").is_some_and(|v| v == "1"))
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum Record {
    Span {
        handkite_timing: u8,
        pid: u32,
        operation: u64,
        stage: &'static str,
        started_us: u128,
        elapsed_us: u128,
    },
    Capture {
        handkite_timing: u8,
        pid: u32,
        operation: u64,
        capture: CaptureTimings,
    },
}

fn emit(record: Record) {
    let sink = SINK.get_or_init(|| {
        let (sender, receiver) = sync_channel::<Envelope>(1024);
        std::thread::Builder::new()
            .name("handkite-timings".into())
            .spawn(move || {
                for record in receiver {
                    // A slow or broken diagnostic reader must never delay device input.
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "{}",
                        serde_json::to_string(&record).unwrap_or_default()
                    );
                }
            })
            .ok()
            .map(|_| sender)
    });
    if let Some(sink) = sink {
        let _ = sink.try_send(Envelope {
            sequence: SEQUENCE.fetch_add(1, Ordering::Relaxed),
            record,
        });
    }
}

pub(crate) struct Span {
    started: Option<Instant>,
    stage: &'static str,
    operation: u64,
}

impl Span {
    pub(crate) fn operation(stage: &'static str) -> Self {
        if enabled() {
            OPERATION.fetch_add(1, Ordering::Relaxed);
        }
        Self::new(stage)
    }

    pub(crate) fn new(stage: &'static str) -> Self {
        Self {
            started: enabled().then(|| {
                let now = Instant::now();
                EPOCH.get_or_init(|| now);
                now
            }),
            stage,
            operation: OPERATION.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let finished = Instant::now();
            let epoch = *EPOCH.get().expect("enabled span initialized epoch");
            emit(Record::Span {
                handkite_timing: 1,
                pid: std::process::id(),
                operation: self.operation,
                stage: self.stage,
                started_us: started.saturating_duration_since(epoch).as_micros(),
                elapsed_us: finished.saturating_duration_since(started).as_micros(),
            });
        }
    }
}

pub(crate) fn capture(capture: CaptureTimings) {
    emit(Record::Capture {
        handkite_timing: 1,
        pid: std::process::id(),
        operation: OPERATION.load(Ordering::Relaxed),
        capture,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_has_only_fixed_metadata_and_durations() {
        let row = serde_json::to_value(Record::Span {
            handkite_timing: 1,
            pid: 9,
            operation: 7,
            stage: "wda_response",
            started_us: 12,
            elapsed_us: 345,
        })
        .unwrap();
        assert_eq!(row.as_object().unwrap().len(), 6);
        assert_eq!(row["stage"], "wda_response");
        assert_eq!(row["operation"], 7);
        assert_eq!(row["started_us"], 12);
        assert_eq!(row["elapsed_us"], 345);
        assert_eq!(row["handkite_timing"], 1);
    }

    #[test]
    fn saturated_queue_drops_without_waiting_for_reader() {
        let (sender, _receiver) = sync_channel(1);
        sender.try_send(1).unwrap();
        assert!(matches!(
            sender.try_send(2),
            Err(std::sync::mpsc::TrySendError::Full(2))
        ));
    }
}
