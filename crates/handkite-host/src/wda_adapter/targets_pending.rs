//! Owned, single-flight post-input proof. Pending frames never authorize input.
//!
//! An acknowledged tap starts no further device work. The next incremental
//! observation evaluates the same identity, state, focus, descriptor and region
//! evidence as the standalone effect sample. A disappearance still needs the
//! exact cached-ID stale response: it runs while the controller is idle, before
//! a target overlapping the acted region, or before any full capture. A proof
//! superseded by a non-overlapping action reports Unconfirmed. Nothing is
//! replayed or promoted into input authority.
use super::*;
use handkite_core::controller::{TargetCompletion, TargetEffect, TargetEffectReport, TextReadback};
use handkite_core::protocol::ObservedEffect;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// WDA's element cache holds 1024 LRU entries and the acknowledged click made
/// the acted ID most recent. Fewer new entries than this cannot evict it, so a
/// later stale response still describes that element.
const EVICTION_BOUND: usize = 512;
/// Controller idle time before a background stale-ID request may claim WDA.
const IDLE_GRACE: Duration = Duration::from_millis(1500);
const WAITING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

enum Verdict {
    Complete(RawImageObservation, bool, LockState, Instant, u64),
    Unconfirmed,
    Budget,
    RestorationFailed,
}

enum Stage {
    /// Acknowledged; the next observation supplies the sample.
    Deferred(Box<Input>),
    /// Rows and pixels show disappearance; `rows` counts cache insertions since
    /// the click. Only the stale-ID response remains.
    Absent {
        input: Box<Input>,
        rows: usize,
        idle: Option<Idle>,
    },
    /// A spawned worker; fixtures model worker failures with it.
    #[cfg_attr(not(test), allow(dead_code))]
    Running(tokio::task::JoinHandle<Verdict>),
}

struct Idle {
    claim: Arc<AtomicU8>,
    task: tokio::task::JoinHandle<Option<Verdict>>,
}

pub(in crate::wda_adapter) struct PendingEffect {
    key: cache::Key,
    token: u64,
    stage: Stage,
}

fn image(raw: RawObservation) -> RawImageObservation {
    RawImageObservation {
        connected_usb: raw.connected_usb,
        wda_target: raw.wda_target,
        window: raw.window,
        screenshot: raw.screenshot,
    }
}

async fn frame(wda: &UsbWda, input: &Input) -> Result<RawObservation, AdapterError> {
    let _span = crate::timing::Span::new("target_provisional_capture");
    let mapping = input.bound.mapping.ok_or(AdapterError)?;
    let started_unix_ms = unix_ms()?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    let (image, app) = tokio::join!(
        screenshot(wda),
        app_matches(wda, &input.key.session, &input.app)
    );
    app?;
    let image = image?;
    super::super::targets_pixels::Pixels::decode(&image.bytes)?.validate_mapping(
        mapping.viewport,
        mapping.scale,
        [mapping.viewport[2], mapping.viewport[3]],
    )?;
    super::super::display::verify_selection(wda, &input.key.session, mapping.display_id).await?;
    Ok(RawObservation {
        connected_usb: input.key.device.clone(),
        wda_target: input.key.device.clone(),
        window: ObservationWindow {
            started_unix_ms,
            finished_unix_ms: unix_ms()?,
        },
        state: DeviceState {
            lock: LockState::Unlocked,
            wda_ready: true,
            foreground_app: Some(input.app.clone()),
        },
        accessibility_tree: String::new(),
        screenshot: image,
    })
}

/// The complete standalone sample, used when no incremental capture carried it.
async fn full(wda: UsbWda, input: Input) -> Verdict {
    let _span = crate::timing::Span::new("target_effect_background");
    let device = input.key.device.clone();
    let started = Instant::now();
    let (lock, result) = capture_with_native(
        move || native_lock::query(&device).state,
        target_context(&wda, &input.key.session, fast::after(&wda, &input)),
    )
    .await;
    match result {
        Err(_) => Verdict::RestorationFailed,
        Ok(Err(TargetWorkError::Budget)) => Verdict::Budget,
        Ok(Err(TargetWorkError::Capture)) => Verdict::Unconfirmed,
        Ok(Ok((raw, _, verified, _))) => match unix_ms() {
            Ok(completed) => Verdict::Complete(image(raw), verified, lock, started, completed),
            Err(_) => Verdict::Unconfirmed,
        },
    }
}

/// The exact cached-ID stale response plus a fresh frame whose target region
/// still differs from the pre-input region.
async fn stale(wda: UsbWda, input: Input) -> Verdict {
    let _span = crate::timing::Span::new("target_effect_stale");
    let device = input.key.device.clone();
    let started = Instant::now();
    let (lock, result) =
        capture_with_native(move || native_lock::query(&device).state, async {
            let gone = fast::absent(&wda, &input).await?;
            let raw = frame(&wda, &input).await?;
            let mapping = input.bound.mapping.ok_or(AdapterError)?;
            let region = super::super::targets_pixels::Pixels::decode(&raw.screenshot.bytes)?
                .region(mapping.viewport, &input.bound.attrs, &mut 0)?;
            let changed = input
                .bound
                .region
                .as_ref()
                .is_some_and(|before| before != &region);
            Ok((raw, gone && changed))
        })
        .await;
    match (result, unix_ms()) {
        (Ok((raw, verified)), Ok(completed)) => {
            Verdict::Complete(image(raw), verified, lock, started, completed)
        }
        _ => Verdict::Unconfirmed,
    }
}

impl WdaAdapter {
    fn effect_wda(&self) -> Result<UsbWda, AdapterError> {
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        Ok(UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?)))
    }

    pub(in crate::wda_adapter) fn target_rect(&self, target_id: &str) -> Option<[f64; 4]> {
        self.targets.as_ref().and_then(|targets| {
            targets
                .entries
                .iter()
                .find(|e| e.public.target_id == target_id)
                .map(|e| e.public.rect)
        })
    }

    pub(in crate::wda_adapter) fn defer_scoped_tap(&mut self, input: Input) -> TargetCompletion {
        self.target_snapshot = None;
        self.targets = None;
        let capture = (|| {
            self.validate_owned()?;
            if self.pending_effect.is_some() || self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            let wda = self.effect_wda()?;
            let device = input.key.device.clone();
            let native_started = Instant::now();
            let (lock, raw) =
                self.runtime
                    .as_ref()
                    .expect("adapter runtime")
                    .block_on(capture_with_native(
                        move || native_lock::query(&device).state,
                        frame(&wda, &input),
                    ));
            self.cache
                .refresh_native(Some(input.key.clone()), native_started, || lock);
            let raw = raw?;
            self.validate_owned()?;
            if lock != LockState::Unlocked || self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            let update = TargetEffect {
                token: input.token,
                target_id: input.bound.public.target_id.clone(),
                effect: ObservedEffect::Pending,
                region: input.bound.public.rect,
            };
            self.pending_effect = Some(PendingEffect {
                key: input.key.clone(),
                token: input.token,
                stage: Stage::Deferred(Box::new(input)),
            });
            if self.effect_reports.len() == 4 {
                self.effect_reports.remove(0);
            }
            self.effect_reports.push(TargetEffectReport {
                update,
                proof: None,
                completed_unix_ms: 0,
            });
            Ok(raw)
        })();
        TargetCompletion {
            readback: capture.map(|raw| TextReadback {
                image: image(raw),
                input_verified: false,
            }),
            // A provisional image is not a target inventory or actionable revision.
            next: Err(AdapterError),
        }
    }

    fn spawn_and_wait<F>(&self, work: F) -> Option<Verdict>
    where
        F: Future<Output = Verdict> + Send + 'static,
    {
        let runtime = self.runtime.as_ref().expect("adapter runtime");
        runtime.block_on(runtime.spawn(work)).ok()
    }

    /// Settle claimed idle work. `Err` carries a stale verdict already obtained;
    /// `Ok` means no request was or will be sent by that idle task.
    fn cancel_idle(&self, idle: Idle) -> Result<(), Option<Verdict>> {
        if idle
            .claim
            .compare_exchange(WAITING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            idle.task.abort();
            return Ok(());
        }
        let runtime = self.runtime.as_ref().expect("adapter runtime");
        match runtime.block_on(idle.task) {
            Ok(Some(verdict)) => Err(Some(verdict)),
            Ok(None) => Ok(()),
            Err(_) => Err(None),
        }
    }

    /// `None` means a worker failed and admission is poisoned.
    fn resolve(&self, stage: Stage, run: bool) -> Option<Verdict> {
        match stage {
            Stage::Running(task) => self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .block_on(task)
                .ok(),
            Stage::Deferred(input) => match (run, self.effect_wda()) {
                (true, Ok(wda)) => self.spawn_and_wait(full(wda, *input)),
                _ => Some(Verdict::Unconfirmed),
            },
            Stage::Absent { input, rows, idle } => {
                if let Some(idle) = idle
                    && let Err(verdict) = self.cancel_idle(idle)
                {
                    return verdict;
                }
                match (run && rows < EVICTION_BOUND, self.effect_wda()) {
                    (true, Ok(wda)) => self.spawn_and_wait(stale(wda, *input)),
                    _ => Some(Verdict::Unconfirmed),
                }
            }
        }
    }

    fn report(
        &mut self,
        key: cache::Key,
        token: u64,
        verdict: Option<Verdict>,
    ) -> Result<(), AdapterError> {
        let valid = self.validate_owned().is_ok() && self.cache_key().as_ref() == Some(&key);
        let mut proof = None;
        let mut verified = false;
        let mut poisoned = false;
        let mut budget = false;
        let mut completed = 0;
        match verdict {
            Some(Verdict::Complete(raw, effect, lock, started, finished)) => {
                completed = finished;
                self.cache.refresh_native(Some(key), started, || lock);
                verified = valid && effect && lock == LockState::Unlocked;
                proof = Some(Arc::new(raw));
            }
            Some(Verdict::Unconfirmed) => {}
            Some(Verdict::Budget) => budget = true,
            Some(Verdict::RestorationFailed) | None => poisoned = true,
        }
        if let Some(report) = self
            .effect_reports
            .iter_mut()
            .find(|r| r.update.token == token)
        {
            report.update.effect = if verified {
                ObservedEffect::Changed
            } else {
                ObservedEffect::Unconfirmed
            };
            report.proof = proof;
            report.completed_unix_ms = completed;
        }
        if poisoned {
            self.text_response_poisoned = true;
            self.error_diagnostic = Some(AdapterDiagnostic::TargetSettings);
            return Err(AdapterError);
        }
        if budget {
            self.clear_budget_targets();
            self.error_diagnostic = Some(AdapterDiagnostic::TargetBudget);
            return Err(AdapterError);
        }
        Ok(())
    }

    pub(in crate::wda_adapter) fn finish_pending_effect(
        &mut self,
        wait: bool,
    ) -> Result<(), AdapterError> {
        let Some(pending) = self.pending_effect.as_ref() else {
            return Ok(());
        };
        if !wait {
            let ready = match &pending.stage {
                Stage::Running(task) => task.is_finished(),
                Stage::Absent {
                    idle: Some(idle), ..
                } => idle.task.is_finished() && idle.claim.load(Ordering::SeqCst) == CLAIMED,
                _ => false,
            };
            if !ready {
                return Ok(());
            }
        }
        let pending = self.pending_effect.take().expect("checked pending");
        let verdict = self.resolve(pending.stage, true);
        self.report(pending.key, pending.token, verdict)
    }

    /// Teardown and supersession send no new proof request.
    pub(in crate::wda_adapter) fn abandon_pending_effect(&mut self) -> Result<(), AdapterError> {
        let Some(pending) = self.pending_effect.take() else {
            return Ok(());
        };
        let verdict = self.resolve(pending.stage, false);
        self.report(pending.key, pending.token, verdict)
    }

    /// Before one element action: wait only for a proof about a region the
    /// target overlaps. Any other pending proof is superseded as Unconfirmed.
    pub(in crate::wda_adapter) fn settle_for_target(
        &mut self,
        rect: Option<[f64; 4]>,
    ) -> Result<(), AdapterError> {
        let disjoint = match (&self.pending_effect, rect) {
            (
                Some(PendingEffect {
                    stage: Stage::Absent { input, .. },
                    ..
                }),
                Some(rect),
            ) => !delta::intersects(input.bound.public.rect, rect),
            _ => false,
        };
        if disjoint {
            self.abandon_pending_effect()
        } else {
            self.finish_pending_effect(true)
        }
    }

    /// Before an incremental capture: the acted input whose sample this capture
    /// may carry. Running or claimed proof is drained first.
    pub(in crate::wda_adapter) fn pending_for_delta(
        &mut self,
        key: &cache::Key,
    ) -> Result<Option<Input>, AdapterError> {
        let Some(pending) = self.pending_effect.as_mut() else {
            return Ok(None);
        };
        if &pending.key != key || matches!(pending.stage, Stage::Running(_)) {
            return self.finish_pending_effect(true).map(|_| None);
        }
        if let Stage::Absent { idle, .. } = &mut pending.stage
            && let Some(idle) = idle.take()
            && let Err(verdict) = self.cancel_idle(idle)
        {
            let pending = self.pending_effect.take().expect("checked pending");
            return self
                .report(pending.key, pending.token, verdict)
                .map(|_| None);
        }
        Ok(
            match &self.pending_effect.as_ref().expect("checked pending").stage {
                Stage::Deferred(input) | Stage::Absent { input, .. } => Some((**input).clone()),
                Stage::Running(_) => None,
            },
        )
    }

    /// Record the effect carried by one successful, Unlocked incremental capture.
    pub(super) fn record_delta_effect(
        &mut self,
        outcome: fast::Outcome,
        inserted: usize,
        proof: RawImageObservation,
        started: Instant,
    ) -> Result<(), AdapterError> {
        let Some(pending) = self.pending_effect.take() else {
            return Ok(());
        };
        let (input, rows) = match pending.stage {
            Stage::Deferred(input) => (input, inserted),
            Stage::Absent { input, rows, .. } => (input, rows.saturating_add(inserted)),
            stage @ Stage::Running(_) => {
                let verdict = self.resolve(stage, true);
                return self.report(pending.key, pending.token, verdict);
            }
        };
        let verdict = match outcome {
            fast::Outcome::Changed => {
                Verdict::Complete(proof, true, LockState::Unlocked, started, unix_ms()?)
            }
            fast::Outcome::Absent if rows < EVICTION_BOUND => {
                let idle = self.arm_idle(&input);
                self.pending_effect = Some(PendingEffect {
                    key: pending.key,
                    token: pending.token,
                    stage: Stage::Absent { input, rows, idle },
                });
                return Ok(());
            }
            fast::Outcome::Absent | fast::Outcome::Unconfirmed => Verdict::Unconfirmed,
        };
        self.report(pending.key, pending.token, Some(verdict))
    }

    fn arm_idle(&self, input: &Input) -> Option<Idle> {
        let wda = self.effect_wda().ok()?;
        let claim = Arc::new(AtomicU8::new(WAITING));
        let worker = claim.clone();
        let input = input.clone();
        let task = self.runtime.as_ref()?.spawn(async move {
            tokio::time::sleep(IDLE_GRACE).await;
            worker
                .compare_exchange(WAITING, CLAIMED, Ordering::SeqCst, Ordering::SeqCst)
                .ok()?;
            Some(stale(wda, input).await)
        });
        Some(Idle { claim, task })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn pending(adapter: &mut WdaAdapter, outcome: Verdict, completed: Arc<AtomicBool>) {
        let task = adapter.runtime.as_ref().unwrap().spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            completed.store(true, Ordering::SeqCst);
            outcome
        });
        let key = cache::Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "session".into(),
            runner: task.id(),
        };
        adapter.effect_reports.push(TargetEffectReport {
            update: TargetEffect {
                token: 1,
                target_id: "t_0123456789abcdef0123456789abcdef".into(),
                effect: ObservedEffect::Pending,
                region: [0., 0., 1., 1.],
            },
            proof: None,
            completed_unix_ms: 0,
        });
        adapter.pending_effect = Some(PendingEffect {
            key,
            token: 1,
            stage: Stage::Running(task),
        });
    }

    #[test]
    fn polling_retains_pending_and_terminal_reports_without_replaying() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        pending(&mut adapter, Verdict::Unconfirmed, completed.clone());
        adapter.finish_pending_effect(false).unwrap();
        assert!(!completed.load(Ordering::SeqCst));
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Pending
        );
        adapter.finish_pending_effect(true).unwrap();
        assert!(completed.load(Ordering::SeqCst));
        assert!(adapter.pending_effect.is_none());
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
        adapter.finish_pending_effect(false).unwrap();
        assert_eq!(adapter.effect_reports.len(), 1);
    }

    #[test]
    fn restoration_failure_poison_is_preserved() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        pending(
            &mut adapter,
            Verdict::RestorationFailed,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(adapter.finish_pending_effect(true).is_err());
        assert!(adapter.text_response_poisoned);
        assert!(adapter.validate_owned().is_err());
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
    }

    #[test]
    fn budget_refusal_preserves_acknowledged_history_without_poison_or_replay() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        pending(
            &mut adapter,
            Verdict::Budget,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(adapter.finish_pending_effect(true).is_err());
        assert!(adapter.pending_effect.is_none());
        assert!(!adapter.text_response_poisoned);
        assert_eq!(
            adapter.error_diagnostic,
            Some(AdapterDiagnostic::TargetBudget)
        );
        assert_eq!(adapter.effect_reports.len(), 1);
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
        adapter.finish_pending_effect(true).unwrap();
        assert_eq!(adapter.effect_reports.len(), 1);
    }

    #[test]
    fn delta_budget_ends_deferred_proof_without_a_runner_or_new_capture() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let task_id = adapter.runtime.as_ref().unwrap().spawn(async {}).id();
        let input = fast::tests::input(task_id);
        adapter.effect_reports.push(TargetEffectReport {
            update: TargetEffect {
                token: input.token,
                target_id: input.bound.public.target_id.clone(),
                effect: ObservedEffect::Pending,
                region: input.bound.public.rect,
            },
            proof: None,
            completed_unix_ms: 0,
        });
        adapter.pending_effect = Some(PendingEffect {
            key: input.key.clone(),
            token: input.token,
            stage: Stage::Deferred(Box::new(input)),
        });
        assert_eq!(
            adapter.target_work::<()>(Err(TargetWorkError::Budget)),
            Err(AdapterError)
        );
        assert!(adapter.pending_effect.is_none());
        assert!(!adapter.text_response_poisoned);
        assert_eq!(
            adapter.error_diagnostic,
            Some(AdapterDiagnostic::TargetBudget)
        );
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
        adapter.finish_pending_effect(true).unwrap();
        assert_eq!(adapter.effect_reports.len(), 1);
    }

    #[test]
    fn budget_refusal_cannot_hide_an_inflight_restoration_failure() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        pending(
            &mut adapter,
            Verdict::RestorationFailed,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            adapter.target_work::<()>(Err(TargetWorkError::Budget)),
            Err(AdapterError)
        );
        assert!(adapter.text_response_poisoned);
        assert_eq!(
            adapter.error_diagnostic,
            Some(AdapterDiagnostic::TargetSettings)
        );
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
    }

    #[test]
    fn cancelled_worker_cannot_silently_leave_the_profile_trusted() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        pending(
            &mut adapter,
            Verdict::Unconfirmed,
            Arc::new(AtomicBool::new(false)),
        );
        let Stage::Running(task) = &adapter.pending_effect.as_ref().unwrap().stage else {
            unreachable!()
        };
        task.abort();
        assert!(adapter.finish_pending_effect(true).is_err());
        assert!(adapter.text_response_poisoned);
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
    }

    #[test]
    fn teardown_drains_the_worker_before_dropping_the_runtime() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let completed = Arc::new(AtomicBool::new(false));
        pending(&mut adapter, Verdict::Unconfirmed, completed.clone());
        drop(adapter);
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn input_and_full_capture_fences_preserve_restoration_failure() {
        for route in 0..5 {
            let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
            pending(
                &mut adapter,
                Verdict::RestorationFailed,
                Arc::new(AtomicBool::new(false)),
            );
            let refused = match route {
                0 => DeviceAdapter::observe(&mut adapter).is_err(),
                1 => DeviceAdapter::observe_targets(&mut adapter).is_err(),
                2 => DeviceAdapter::observe_navigation(&mut adapter).is_err(),
                3 => DeviceAdapter::prepare_navigation(&mut adapter, &Action::Home).is_err(),
                _ => DeviceAdapter::execute(
                    &mut adapter,
                    &Action::Home,
                    Instant::now() + Duration::from_secs(1),
                )
                .is_err(),
            };
            assert!(refused);
            assert!(adapter.text_response_poisoned);
            assert!(adapter.pending_effect.is_none());
        }
    }

    fn absent(adapter: &mut WdaAdapter, verdict: Verdict, delay: Duration) -> Arc<AtomicU8> {
        let task_id = adapter.runtime.as_ref().unwrap().spawn(async {}).id();
        let input = fast::tests::input(task_id);
        let claim = Arc::new(AtomicU8::new(WAITING));
        let worker = claim.clone();
        let task = adapter.runtime.as_ref().unwrap().spawn(async move {
            tokio::time::sleep(delay).await;
            worker
                .compare_exchange(WAITING, CLAIMED, Ordering::SeqCst, Ordering::SeqCst)
                .ok()?;
            Some(verdict)
        });
        adapter.effect_reports.push(TargetEffectReport {
            update: TargetEffect {
                token: 1,
                target_id: input.bound.public.target_id.clone(),
                effect: ObservedEffect::Pending,
                region: input.bound.public.rect,
            },
            proof: None,
            completed_unix_ms: 0,
        });
        adapter.pending_effect = Some(PendingEffect {
            key: input.key.clone(),
            token: 1,
            stage: Stage::Absent {
                input: Box::new(input),
                rows: 1,
                idle: Some(Idle {
                    claim: claim.clone(),
                    task,
                }),
            },
        });
        claim
    }

    #[test]
    fn disjoint_target_supersedes_an_unsent_stale_proof() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let claim = absent(
            &mut adapter,
            Verdict::RestorationFailed,
            Duration::from_secs(5),
        );
        adapter.settle_for_target(Some([3., 6., 1., 1.])).unwrap();
        assert_eq!(claim.load(Ordering::SeqCst), CANCELLED);
        assert!(adapter.pending_effect.is_none());
        assert!(!adapter.text_response_poisoned);
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
    }

    #[test]
    fn claimed_stale_proof_is_drained_by_overlapping_and_disjoint_targets() {
        for rect in [[1., 1., 1., 1.], [3., 6., 1., 1.]] {
            let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
            let claim = absent(&mut adapter, Verdict::RestorationFailed, Duration::ZERO);
            // The fixture runtime is current-thread: drive it until the claim.
            while claim.load(Ordering::SeqCst) != CLAIMED {
                adapter
                    .runtime
                    .as_ref()
                    .unwrap()
                    .block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            }
            // The claimed worker's own verdict is collected, never discarded.
            assert!(adapter.settle_for_target(Some(rect)).is_err());
            assert!(adapter.text_response_poisoned);
            assert!(adapter.pending_effect.is_none());
        }
    }

    #[test]
    fn eviction_bound_and_unconfirmed_samples_end_the_proof_without_requests() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        let claim = absent(
            &mut adapter,
            Verdict::RestorationFailed,
            Duration::from_secs(5),
        );
        let key = adapter.pending_effect.as_ref().unwrap().key.clone();
        // A delta first cancels the unclaimed idle request, then counts its rows.
        assert!(adapter.pending_for_delta(&key).unwrap().is_some());
        assert_eq!(claim.load(Ordering::SeqCst), CANCELLED);
        let proof = RawImageObservation {
            connected_usb: HardwareId::new("fixture").unwrap(),
            wda_target: HardwareId::new("fixture").unwrap(),
            window: ObservationWindow {
                started_unix_ms: 1,
                finished_unix_ms: 2,
            },
            screenshot: ScreenImage {
                format: handkite_core::protocol::ImageFormat::Png,
                bytes: vec![1],
            },
        };
        adapter
            .record_delta_effect(fast::Outcome::Absent, EVICTION_BOUND, proof, Instant::now())
            .unwrap();
        assert!(adapter.pending_effect.is_none());
        assert_eq!(
            adapter.effect_reports[0].update.effect,
            ObservedEffect::Unconfirmed
        );
    }
}
