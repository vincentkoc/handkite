//! Owned, single-flight post-input proof. Pending frames never authorize input.
use super::*;
use handkite_core::controller::{TargetCompletion, TargetEffect, TargetEffectReport, TextReadback};
use handkite_core::protocol::ObservedEffect;

enum Verdict {
    Complete(RawObservation, bool, LockState, Instant, u64),
    Unconfirmed,
    RestorationFailed,
}

pub(in crate::wda_adapter) struct PendingEffect {
    key: cache::Key,
    token: u64,
    task: tokio::task::JoinHandle<Verdict>,
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

impl WdaAdapter {
    pub(in crate::wda_adapter) fn defer_scoped_tap(&mut self, input: Input) -> TargetCompletion {
        self.target_snapshot = None;
        self.targets = None;
        let capture = (|| {
            self.validate_owned()?;
            if self.pending_effect.is_some() || self.cache_key().as_ref() != Some(&input.key) {
                return Err(AdapterError);
            }
            let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
            let wda = UsbWda::new(self.usb.clone())
                .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
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
            let key = input.key.clone();
            let token = input.token;
            let verification_span = crate::timing::Span::new("target_effect_background");
            let task = self
                .runtime
                .as_ref()
                .expect("adapter runtime")
                .spawn(async move {
                    let _span = verification_span;
                    let device = input.key.device.clone();
                    let started = Instant::now();
                    let (lock, result) = capture_with_native(
                        move || native_lock::query(&device).state,
                        rich_context(&wda, &input.key.session, ATTRS, fast::after(&wda, &input)),
                    )
                    .await;
                    match result {
                        Err(_) => Verdict::RestorationFailed,
                        Ok(Err(_)) => Verdict::Unconfirmed,
                        Ok(Ok((raw, _, verified, _))) => match unix_ms() {
                            Ok(completed) => {
                                Verdict::Complete(raw, verified, lock, started, completed)
                            }
                            Err(_) => Verdict::Unconfirmed,
                        },
                    }
                });
            self.pending_effect = Some(PendingEffect { key, token, task });
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
                image: RawImageObservation {
                    connected_usb: raw.connected_usb,
                    wda_target: raw.wda_target,
                    window: raw.window,
                    screenshot: raw.screenshot,
                },
                input_verified: false,
            }),
            // A provisional image is not a target inventory or actionable revision.
            next: Err(AdapterError),
        }
    }

    pub(in crate::wda_adapter) fn finish_pending_effect(
        &mut self,
        wait: bool,
    ) -> Result<(), AdapterError> {
        let Some(pending) = self.pending_effect.as_ref() else {
            return Ok(());
        };
        if !wait && !pending.task.is_finished() {
            return Ok(());
        }
        let pending = self.pending_effect.take().expect("checked pending");
        let result = self
            .runtime
            .as_ref()
            .expect("adapter runtime")
            .block_on(pending.task);
        let valid =
            self.validate_owned().is_ok() && self.cache_key().as_ref() == Some(&pending.key);
        let mut proof = None;
        let mut verified = false;
        let mut poisoned = false;
        let mut completed = 0;
        match result {
            Ok(Verdict::Complete(raw, effect, lock, started, finished)) => {
                completed = finished;
                self.cache
                    .refresh_native(Some(pending.key), started, || lock);
                verified = valid && effect && lock == LockState::Unlocked;
                proof = Some(std::sync::Arc::new(RawImageObservation {
                    connected_usb: raw.connected_usb,
                    wda_target: raw.wda_target,
                    window: raw.window,
                    screenshot: raw.screenshot,
                }));
            }
            Ok(Verdict::Unconfirmed) => {}
            Ok(Verdict::RestorationFailed) | Err(_) => poisoned = true,
        }
        if let Some(report) = self
            .effect_reports
            .iter_mut()
            .find(|r| r.update.token == pending.token)
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
        Ok(())
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
            task,
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
    fn cancelled_worker_cannot_silently_leave_the_profile_trusted() {
        let mut adapter = WdaAdapter::new(HardwareId::new("fixture").unwrap()).unwrap();
        pending(
            &mut adapter,
            Verdict::Unconfirmed,
            Arc::new(AtomicBool::new(false)),
        );
        adapter.pending_effect.as_ref().unwrap().task.abort();
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
}
