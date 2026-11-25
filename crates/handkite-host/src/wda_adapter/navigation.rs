use super::*;

const HOME_PROOF_HANDOFF: Duration = Duration::from_millis(50);

pub(super) struct HomeProof {
    key: cache::Key,
    native: LockState,
    context: (String, bool),
    started_unix_ms: u64,
    finished: Instant,
}

impl HomeProof {
    fn matches(&self, key: Option<&cache::Key>, now: Instant) -> bool {
        key == Some(&self.key)
            && self.native == LockState::Unlocked
            && self.context.0 == "com.apple.springboard"
            && !self.context.1
            && now
                .checked_duration_since(self.finished)
                .is_some_and(|age| age < HOME_PROOF_HANDOFF)
    }
}

fn reusable_home_context(
    native_before: LockState,
    native_after: LockState,
    before: &(String, bool),
    after: &(String, bool),
    reported: Option<bool>,
) -> bool {
    reported == Some(false)
        && native_before == LockState::Unlocked
        && native_after == LockState::Unlocked
        && !before.0.is_empty()
        && !before.1
        && after.0 == "com.apple.springboard"
        && !after.1
}

impl WdaAdapter {
    pub(super) fn navigation_frame(&mut self) -> Result<RawObservation, AdapterError> {
        self.home_readback_proof = None;
        only_observation(self.navigation_frame_with_proof(None, false, None)?)
    }

    pub(super) fn home_navigation_frame(
        &mut self,
        retry: bool,
    ) -> Result<RawObservation, AdapterError> {
        let proof = self.home_readback_proof.take().filter(|_| retry);
        only_observation(self.navigation_frame_with_proof(proof, !retry, None)?)
    }

    pub(super) fn launch_navigation_frame(
        &mut self,
        source: &str,
        destination: &str,
    ) -> Result<LaunchReadback, AdapterError> {
        self.navigation_frame_with_proof(None, false, Some((source, destination)))
    }

    fn navigation_frame_with_proof(
        &mut self,
        proof: Option<HomeProof>,
        allow_handoff: bool,
        launch: Option<(&str, &str)>,
    ) -> Result<LaunchReadback, AdapterError> {
        self.error_diagnostic = None;
        self.cache.clear_full();
        let mut started_unix_ms = unix_ms()?;
        self.validate_owned()?;
        let initial_key = self.cache_key();
        if self.owned_session.is_none() || self.owned_runner.is_none() {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let session = self.owned_session.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone());
        let (native_before, before) = {
            let _timing = crate::timing::Span::new("navigation_before_proof");
            let proof =
                proof.filter(|proof| proof.matches(self.cache_key().as_ref(), Instant::now()));
            crate::timing::outcome("navigation_before_proof_reused", proof.is_some());
            if let Some(proof) = proof {
                // Keep the original evidence window, never a fresh handoff timestamp.
                started_unix_ms = proof.started_unix_ms;
                (proof.native, Ok(proof.context))
            } else {
                self.navigation_context(&wda, session)?
            }
        };
        if native_before == LockState::Uncertain {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let before = before?;
        // Both before proofs complete before the screenshot starts. Neither
        // after proof starts until that screenshot has completed.
        let frame = {
            let _timing = crate::timing::Span::new("navigation_capture");
            self.capture_frame(None)?
        };
        let after_started_unix_ms = unix_ms()?;
        let (native_after, after) = {
            let _timing = crate::timing::Span::new("navigation_after_proof");
            self.navigation_context(&wda, session)?
        };
        let proof_finished = Instant::now();
        let after = after?;
        let coherent = same_context(&before, &frame.app, frame.reported, &after);
        crate::timing::outcome("home_proof_allowed", allow_handoff);
        if allow_handoff {
            for (stage, value) in [
                (
                    "home_proof_frame_home",
                    frame.app == "com.apple.springboard",
                ),
                ("home_proof_frame_unlocked", frame.reported == Some(false)),
                ("home_proof_before_known", !before.0.is_empty()),
                ("home_proof_before_unlocked", !before.1),
                ("home_proof_after_home", after.0 == "com.apple.springboard"),
                ("home_proof_after_unlocked", !after.1),
                (
                    "home_proof_native_before",
                    native_before == LockState::Unlocked,
                ),
                (
                    "home_proof_native_after",
                    native_after == LockState::Unlocked,
                ),
                ("home_proof_key_available", self.cache_key().is_some()),
            ] {
                crate::timing::outcome(stage, value);
            }
        }
        if allow_handoff
            && !coherent
            && reusable_home_context(native_before, native_after, &before, &after, frame.reported)
        {
            // Discard this frame. Only the later independent after-proof can
            // precede the retry's new frame, after final ownership validation.
            self.validate_owned()?;
            self.home_readback_proof = self.cache_key().map(|key| HomeProof {
                key,
                native: native_after,
                context: after.clone(),
                started_unix_ms: after_started_unix_ms,
                finished: proof_finished,
            });
        }
        crate::timing::outcome(
            "navigation_home_proof_handoff",
            self.home_readback_proof.is_some(),
        );
        crate::timing::outcome("navigation_context_coherent", coherent);
        if let Some((source, destination)) = launch {
            for (stage, value) in [
                ("launch_before_source", before.0 == source),
                ("launch_before_destination", before.0 == destination),
                ("launch_frame_source", frame.app == source),
                ("launch_frame_destination", frame.app == destination),
                ("launch_after_destination", after.0 == destination),
                (
                    "launch_native_unlocked",
                    native_before == LockState::Unlocked && native_after == LockState::Unlocked,
                ),
                (
                    "launch_wda_unlocked",
                    !before.1 && !after.1 && frame.reported == Some(false),
                ),
            ] {
                crate::timing::outcome(stage, value);
            }
        }
        if !coherent
            && launch.is_some_and(|expected| {
                launch_transition(
                    expected,
                    (native_before, native_after),
                    &before,
                    (&frame.app, frame.reported),
                    &after,
                )
            })
        {
            transition_owner(initial_key, || {
                self.validate_owned()?;
                Ok(self.cache_key())
            })?;
            crate::timing::outcome("navigation_launch_transition", true);
            return Ok(LaunchReadback::Transition);
        }
        let state = readback_state(
            native_before,
            native_after,
            &before,
            &frame.app,
            frame.reported,
            &after,
        )?;
        self.validate_owned()?;
        Ok(LaunchReadback::Observation(RawObservation {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            window: ObservationWindow {
                started_unix_ms,
                finished_unix_ms: unix_ms()?,
            },
            state,
            accessibility_tree: String::new(),
            screenshot: frame.screenshot,
        }))
    }

    fn navigation_context(
        &self,
        wda: &impl WdaRequest,
        session: &str,
    ) -> Result<(LockState, Result<(String, bool), AdapterError>), AdapterError> {
        let expected = self.expected.clone();
        let started = Instant::now();
        let (native, context) =
            self.runtime
                .as_ref()
                .ok_or(AdapterError)?
                .block_on(context_with_native(
                    move || native_lock::query(&expected).state,
                    context(wda, session),
                ));
        self.cache.record_native(self.cache_key(), native, started);
        Ok((native, context))
    }

    pub(super) fn navigation_prepare(
        &mut self,
        action: &Action,
    ) -> Result<(RawObservation, ActionAdmission), AdapterError> {
        let _timing = crate::timing::Span::new("navigation_prepare");
        if !matches!(action, Action::Home | Action::LaunchApp { .. }) {
            return Err(AdapterError);
        }
        if let Action::LaunchApp { bundle_id } = action
            && (bundle_id.is_empty()
                || bundle_id.len() > 256
                || bundle_id.chars().any(char::is_whitespace))
        {
            return Err(AdapterError);
        }
        let frame = self.navigation_frame()?;
        if frame.state.lock != LockState::Unlocked {
            return Err(AdapterError);
        }
        let until = Instant::now() + HOME_LAUNCH_AGE;
        let admission = if let Action::LaunchApp { bundle_id } = action {
            self.launch_admission = Some((bundle_id.clone(), until));
            ActionAdmission::NativeAppLaunch { until }
        } else {
            ActionAdmission::NativeHome { until }
        };
        Ok((frame, admission))
    }

    pub(super) fn navigation_execute(
        &mut self,
        action: &Action,
        deadline: Instant,
        admission: ActionAdmission,
        app: &str,
    ) -> Result<TransportReceipt, AdapterError> {
        let _timing = crate::timing::Span::new("navigation_dispatch");
        if !matches!(
            (action, admission),
            (Action::Home, ActionAdmission::NativeHome { .. })
                | (
                    Action::LaunchApp { .. },
                    ActionAdmission::NativeAppLaunch { .. }
                )
        ) {
            return Err(AdapterError);
        }
        self.validate_owned_with_deadline(Some(deadline))?;
        self.execute_in_navigation_context(action, deadline, Some(admission), None, Some(app))
    }

    pub(super) fn native_navigation_input(
        &self,
        app: &str,
        deadline: Instant,
    ) -> Result<LockState, AdapterError> {
        let _timing = crate::timing::Span::new("navigation_final_proof");
        let wda = UsbWda::new(self.usb.clone()).with_write_deadline(deadline);
        let session = self.owned_session.as_ref().ok_or(AdapterError)?;
        let expected = self.expected.clone();
        let started = Instant::now();
        let result = self
            .runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(native_app_before_input(
                move || native_lock::query_until(&expected, deadline).state,
                focused_text::app_matches(&wda, session, app),
                deadline,
            ));
        match &result {
            Ok(native) => self.cache.record_native(self.cache_key(), *native, started),
            Err(_) => self.cache.clear(),
        }
        result
    }
}

fn only_observation(readback: LaunchReadback) -> Result<RawObservation, AdapterError> {
    match readback {
        LaunchReadback::Observation(raw) => Ok(raw),
        LaunchReadback::Transition => Err(AdapterError),
    }
}

fn launch_transition(
    (source, destination): (&str, &str),
    native: (LockState, LockState),
    before: &(String, bool),
    (frame_app, reported): (&str, Option<bool>),
    after: &(String, bool),
) -> bool {
    let allowed_app = |app: &str| app == source || app == destination;
    !source.is_empty()
        && !destination.is_empty()
        && source != destination
        && native == (LockState::Unlocked, LockState::Unlocked)
        && !before.1
        && !after.1
        && reported == Some(false)
        && allowed_app(&before.0)
        && allowed_app(frame_app)
        && after.0 == destination
        && !same_context(before, frame_app, reported, after)
}

fn transition_owner(
    initial: Option<cache::Key>,
    final_check: impl FnOnce() -> Result<Option<cache::Key>, AdapterError>,
) -> Result<(), AdapterError> {
    let initial = initial.ok_or(AdapterError)?;
    (final_check()? == Some(initial))
        .then_some(())
        .ok_or(AdapterError)
}

async fn native_app_before_input(
    native: impl FnOnce() -> LockState + Send + 'static,
    app: impl Future<Output = Result<(), AdapterError>>,
    deadline: Instant,
) -> Result<LockState, AdapterError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(AdapterError)?;
    // Bound the response read itself, independently of the write deadline.
    let app = tokio::time::timeout(remaining, app);
    let (native, app) =
        context_with_native(native, async move { app.await.map_err(|_| AdapterError)? }).await;
    app?;
    if Instant::now() >= deadline {
        return Err(AdapterError);
    }
    Ok(native)
}

async fn context_with_native<T>(
    native: impl FnOnce() -> LockState + Send + 'static,
    context: impl Future<Output = Result<T, AdapterError>>,
) -> (LockState, Result<T, AdapterError>) {
    // Always drain both reads, including when context fails. No proof is
    // published or passed to the next capture stage while a read is pending.
    let (native, context) = tokio::join!(tokio::task::spawn_blocking(native), context);
    (native.unwrap_or(LockState::Uncertain), context)
}

async fn context(wda: &impl WdaRequest, session: &str) -> Result<(String, bool), AdapterError> {
    let app_path = format!("/session/{session}/wda/activeAppInfo");
    let lock_path = format!("/session/{session}/wda/locked");
    let (app, lock) = tokio::join!(
        focused_text::value(wda, session, "GET", &app_path, b""),
        focused_text::value(wda, session, "GET", &lock_path, b"")
    );
    Ok((
        app?["bundleId"].as_str().ok_or(AdapterError)?.to_owned(),
        lock?.as_bool().ok_or(AdapterError)?,
    ))
}
fn same_context(
    before: &(String, bool),
    app: &str,
    lock: Option<bool>,
    after: &(String, bool),
) -> bool {
    before == after && before.0 == app && Some(before.1) == lock
}

pub(super) fn log_launch_retry(
    source: &str,
    destination: &str,
    result: &Result<RawObservation, AdapterError>,
) {
    if !crate::timing::enabled() {
        return;
    }
    crate::timing::outcome("launch_retry_available", result.is_ok());
    if let Ok(raw) = result {
        // Adapter facts only: core identity/window admission still follows.
        for (stage, value) in launch_retry_state(source, destination, &raw.state) {
            crate::timing::outcome(stage, value);
        }
    }
}

fn launch_retry_state(
    source: &str,
    destination: &str,
    state: &DeviceState,
) -> [(&'static str, bool); 7] {
    let app = state
        .foreground_app
        .as_deref()
        .filter(|app| !app.is_empty());
    [
        ("launch_retry_app_known", app.is_some()),
        ("launch_retry_app_source", app == Some(source)),
        ("launch_retry_app_destination", app == Some(destination)),
        (
            "launch_retry_app_other",
            app.is_some_and(|app| app != source && app != destination),
        ),
        (
            "launch_retry_lock_unlocked",
            state.lock == LockState::Unlocked,
        ),
        ("launch_retry_lock_locked", state.lock == LockState::Locked),
        (
            "launch_retry_lock_uncertain",
            state.lock == LockState::Uncertain,
        ),
    ]
}

fn readback_state(
    native_before: LockState,
    native_after: LockState,
    before: &(String, bool),
    app: &str,
    reported: Option<bool>,
    after: &(String, bool),
) -> Result<DeviceState, AdapterError> {
    if !same_context(before, app, reported, after) || native_after != native_before {
        return Err(AdapterError);
    }
    let unlocked = native_before == LockState::Unlocked && reported == Some(false);
    let locked = native_before == LockState::Locked && reported == Some(true);
    if (!unlocked && !locked) || (unlocked && app.is_empty()) {
        return Err(AdapterError);
    }
    Ok(DeviceState {
        lock: native_before,
        wda_ready: true,
        foreground_app: Some(app.to_owned()),
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_read_reports_categories_without_app_content() {
        for (app, expected) in [
            (Some("private.source"), [true, true, false, false]),
            (Some("private.destination"), [true, false, true, false]),
            (Some("private.other"), [true, false, false, true]),
            (Some(""), [false; 4]),
            (None, [false; 4]),
        ] {
            for lock in [LockState::Unlocked, LockState::Locked, LockState::Uncertain] {
                let state = DeviceState {
                    foreground_app: app.map(str::to_owned),
                    lock,
                    wda_ready: true,
                };
                let outcomes = launch_retry_state("private.source", "private.destination", &state);
                assert_eq!(
                    outcomes[..4].iter().map(|(_, ok)| *ok).collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(outcomes[4].1, lock == LockState::Unlocked);
                assert_eq!(outcomes[5].1, lock == LockState::Locked);
                assert_eq!(outcomes[6].1, lock == LockState::Uncertain);
                assert!(outcomes.iter().all(|(stage, _)| !stage.contains("private")));
            }
        }
    }

    #[test]
    fn launch_transition_requires_only_expected_unlocked_apps_and_rejects_old_frame() {
        let old = (String::from("example.source"), false);
        let new = (String::from("example.destination"), false);
        let unlocked = (LockState::Unlocked, LockState::Unlocked);
        let expected = (old.0.as_str(), new.0.as_str());
        for (before, frame) in [
            (&old, old.0.as_str()),
            (&old, new.0.as_str()),
            (&new, old.0.as_str()),
        ] {
            assert!(launch_transition(
                expected,
                unlocked,
                before,
                (frame, Some(false)),
                &new
            ));
            assert!(
                readback_state(
                    LockState::Unlocked,
                    LockState::Unlocked,
                    before,
                    frame,
                    Some(false),
                    &new
                )
                .is_err()
            );
        }
        assert!(!launch_transition(
            expected,
            unlocked,
            &new,
            (&new.0, Some(false)),
            &new
        ));
        for native in [LockState::Locked, LockState::Uncertain] {
            assert!(!launch_transition(
                expected,
                (native, LockState::Unlocked),
                &old,
                (&old.0, Some(false)),
                &new
            ));
            assert!(!launch_transition(
                expected,
                (LockState::Unlocked, native),
                &old,
                (&old.0, Some(false)),
                &new
            ));
        }
        for context in [
            ("other.app".into(), false),
            ("".into(), false),
            (old.0.clone(), true),
        ] {
            assert!(!launch_transition(
                expected,
                unlocked,
                &context,
                (&old.0, Some(false)),
                &new
            ));
            assert!(!launch_transition(
                expected,
                unlocked,
                &old,
                (&old.0, Some(false)),
                &context
            ));
        }
        assert!(!launch_transition(
            expected,
            unlocked,
            &old,
            ("other.app", Some(false)),
            &new
        ));
        assert!(!launch_transition(
            expected,
            unlocked,
            &old,
            (&old.0, Some(false)),
            &(new.0.clone(), true),
        ));
        for reported in [None, Some(true)] {
            assert!(!launch_transition(
                expected,
                unlocked,
                &old,
                (&old.0, reported),
                &new
            ));
        }
        for expected in [
            ("", new.0.as_str()),
            (old.0.as_str(), ""),
            (old.0.as_str(), old.0.as_str()),
        ] {
            assert!(!launch_transition(
                expected,
                unlocked,
                &old,
                (&old.0, Some(false)),
                &new
            ));
        }
        assert!(only_observation(LaunchReadback::Transition).is_err());
    }

    #[test]
    fn launch_transition_requires_final_ownership_and_unchanged_nonempty_key() {
        let key = proof_key();
        assert!(transition_owner(Some(key.clone()), || Ok(Some(key.clone()))).is_ok());
        assert!(transition_owner(None, || panic!("no initial identity")).is_err());
        assert!(transition_owner(Some(key.clone()), || Err(AdapterError)).is_err());
        assert!(transition_owner(Some(key.clone()), || Ok(None)).is_err());
        for changed in [
            cache::Key {
                device: HardwareId::new("other").unwrap(),
                ..key.clone()
            },
            cache::Key {
                session: "other".into(),
                ..key.clone()
            },
            cache::Key {
                runner: proof_key().runner,
                ..key.clone()
            },
        ] {
            assert!(transition_owner(Some(key.clone()), || Ok(Some(changed))).is_err());
        }
    }

    fn proof_key() -> cache::Key {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = runtime.spawn(std::future::pending::<()>());
        let runner = task.id();
        task.abort();
        cache::Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "session".into(),
            runner,
        }
    }

    fn home_proof(key: cache::Key, now: Instant) -> HomeProof {
        HomeProof {
            key,
            native: LockState::Unlocked,
            context: ("com.apple.springboard".into(), false),
            started_unix_ms: 123,
            finished: now,
        }
    }

    #[test]
    fn home_proof_preserves_identity_window_and_strict_handoff_age() {
        let key = proof_key();
        let now = Instant::now();
        let proof = home_proof(key.clone(), now);
        assert!(proof.matches(Some(&key), now));
        assert!(proof.matches(
            Some(&key),
            now + HOME_PROOF_HANDOFF - Duration::from_nanos(1)
        ));
        assert!(!proof.matches(Some(&key), now + HOME_PROOF_HANDOFF));
        assert!(!proof.matches(Some(&key), now - Duration::from_nanos(1)));
        assert!(!proof.matches(None, now));
        let mut changed = key.clone();
        changed.session = "other".into();
        assert!(!proof.matches(Some(&changed), now));
        changed = key.clone();
        changed.device = HardwareId::new("other").unwrap();
        assert!(!proof.matches(Some(&changed), now));
        changed = key.clone();
        changed.runner = proof_key().runner;
        assert!(!proof.matches(Some(&changed), now));
        assert_eq!(proof.started_unix_ms, 123);
        assert_eq!(proof.finished, now);
    }

    #[test]
    fn only_known_unlocked_home_after_context_can_supply_a_proof() {
        let before = ("example.old-app".into(), false);
        let after = ("com.apple.springboard".into(), false);
        assert!(reusable_home_context(
            LockState::Unlocked,
            LockState::Unlocked,
            &before,
            &after,
            Some(false)
        ));
        for native in [LockState::Locked, LockState::Uncertain] {
            assert!(!reusable_home_context(
                native,
                LockState::Unlocked,
                &before,
                &after,
                Some(false)
            ));
            assert!(!reusable_home_context(
                LockState::Unlocked,
                native,
                &before,
                &after,
                Some(false)
            ));
        }
        for context in [
            ("example.app".into(), false),
            ("".into(), false),
            ("com.apple.springboard".into(), true),
        ] {
            assert!(!reusable_home_context(
                LockState::Unlocked,
                LockState::Unlocked,
                &before,
                &context,
                Some(false)
            ));
        }
        for reported in [Some(true), None] {
            assert!(!reusable_home_context(
                LockState::Unlocked,
                LockState::Unlocked,
                &before,
                &after,
                reported
            ));
        }
        assert!(!reusable_home_context(
            LockState::Unlocked,
            LockState::Unlocked,
            &("".into(), false),
            &after,
            Some(false)
        ));
    }

    #[test]
    fn old_frame_is_rejected_but_its_later_proof_can_bracket_a_new_frame() {
        let unlocked = LockState::Unlocked;
        let old = ("example.old-app".into(), false);
        let home = ("com.apple.springboard".into(), false);
        assert!(readback_state(unlocked, unlocked, &old, &old.0, Some(false), &home).is_err());
        assert!(reusable_home_context(
            unlocked,
            unlocked,
            &old,
            &home,
            Some(false)
        ));

        // Only the independent after-proof survives, never the rejected frame.
        let key = proof_key();
        let now = Instant::now();
        let mut slot = Some(home_proof(key.clone(), now));
        let proof = slot.take().filter(|p| p.matches(Some(&key), now)).unwrap();
        assert!(slot.is_none());
        assert_eq!(proof.started_unix_ms, 123);
        assert!(
            readback_state(
                unlocked,
                unlocked,
                &proof.context,
                &home.0,
                Some(false),
                &home
            )
            .is_ok()
        );
        for (app, reported, after, native) in [
            (old.0.as_str(), Some(false), &home, unlocked),
            (home.0.as_str(), Some(false), &old, unlocked),
            (home.0.as_str(), Some(true), &home, unlocked),
            (home.0.as_str(), None, &home, unlocked),
            (home.0.as_str(), Some(false), &home, LockState::Locked),
            (home.0.as_str(), Some(false), &home, LockState::Uncertain),
        ] {
            assert!(
                readback_state(unlocked, native, &proof.context, app, reported, after).is_err()
            );
        }
    }

    #[test]
    fn normal_first_and_failed_retry_calls_consume_any_old_proof_without_io() {
        for mode in 0..3 {
            let key = proof_key();
            let mut adapter = WdaAdapter::new(key.device.clone()).unwrap();
            adapter.home_readback_proof = Some(home_proof(key, Instant::now()));
            // Refuse locally, before any device lookup, on all three entry paths.
            adapter.text_response_poisoned = true;
            let result = match mode {
                0 => adapter.observe_navigation(),
                1 => adapter.observe_home_readback(false),
                _ => adapter.observe_home_readback(true),
            };
            assert!(result.is_err());
            assert!(adapter.home_readback_proof.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unresolved_app_read_times_out_and_drains_native_before_returning() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let drained = Arc::new(AtomicBool::new(false));
        let worker_drained = drained.clone();
        let limit = Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let real_start = Instant::now();
        let proof = tokio::spawn(native_app_before_input(
            move || {
                worker_drained.store(true, Ordering::SeqCst);
                LockState::Unlocked
            },
            std::future::pending(),
            Instant::now() + limit,
        ));
        // Keep the test runnable until the blocking worker has returned; this
        // prevents paused-time auto-advance from racing thread-pool startup.
        while !drained.load(Ordering::SeqCst) {
            assert!(real_start.elapsed() < Duration::from_secs(5));
            tokio::task::yield_now().await;
        }
        tokio::time::advance(limit).await;
        let result = tokio::time::timeout(Duration::from_millis(1), proof)
            .await
            .expect("app read exceeded its deadline")
            .unwrap();
        assert_eq!(result, Err(AdapterError));
        assert!(drained.load(Ordering::SeqCst));
        assert!(started.elapsed() <= limit);
    }

    #[tokio::test(start_paused = true)]
    async fn app_timeout_does_not_detach_a_native_query_still_finishing() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let real_start = Instant::now();
        let limit = Duration::from_secs(10);
        let proof = tokio::spawn(native_app_before_input(
            move || {
                worker_started.store(true, Ordering::SeqCst);
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                LockState::Unlocked
            },
            std::future::pending(),
            Instant::now() + limit,
        ));
        while !started.load(Ordering::SeqCst) {
            assert!(real_start.elapsed() < Duration::from_secs(5));
            tokio::task::yield_now().await;
        }
        tokio::time::advance(limit).await;
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        assert_eq!(proof.await.unwrap(), Err(AdapterError));
    }

    #[tokio::test]
    async fn final_app_check_overlaps_native_but_cannot_send_before_both_finish() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let (app_done, completed) = tokio::sync::oneshot::channel();
        let proof = tokio::spawn(native_app_before_input(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                LockState::Unlocked
            },
            async move {
                app_done.send(()).unwrap();
                Ok(())
            },
            Instant::now() + Duration::from_secs(2),
        ));
        tokio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        assert_eq!(proof.await.unwrap(), Ok(LockState::Unlocked));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_final_app_check_drains_native_and_preserves_refusal() {
        let (release, wait) = std::sync::mpsc::channel();
        let (app_done, completed) = tokio::sync::oneshot::channel();
        let proof = tokio::spawn(native_app_before_input(
            move || {
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                LockState::Unlocked
            },
            async move {
                app_done.send(()).unwrap();
                Err(AdapterError)
            },
            Instant::now() + Duration::from_secs(2),
        ));
        tokio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        assert_eq!(proof.await.unwrap(), Err(AdapterError));
    }

    #[tokio::test]
    async fn final_proof_keeps_locked_uncertain_and_expired_states_unusable() {
        for state in [LockState::Locked, LockState::Uncertain] {
            let result = native_app_before_input(
                move || state,
                async { Ok(()) },
                Instant::now() + Duration::from_secs(1),
            )
            .await;
            assert_eq!(result, Ok(state));
            assert_ne!(result, Ok(LockState::Unlocked));
        }
        assert!(
            native_app_before_input(
                || panic!("expired proof queried native state"),
                async { panic!("expired proof queried app state") },
                Instant::now()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn context_runs_while_native_is_pending_but_cannot_publish_early() {
        let (release, wait) = std::sync::mpsc::channel();
        let (context_done, completed) = tokio::sync::oneshot::channel();
        let proof = tokio::spawn(context_with_native(
            move || {
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                LockState::Unlocked
            },
            async move {
                context_done.send(()).unwrap();
                Ok(("example.app".to_owned(), false))
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        let (native, context) = proof.await.unwrap();
        assert_eq!(native, LockState::Unlocked);
        assert_eq!(context.unwrap(), ("example.app".into(), false));
    }

    #[tokio::test]
    async fn native_failure_still_drains_context_and_cannot_publish_early() {
        let (release, wait) = tokio::sync::oneshot::channel();
        let (native_done, completed) = tokio::sync::oneshot::channel();
        let proof = tokio::spawn(context_with_native(
            move || {
                native_done.send(()).unwrap();
                LockState::Uncertain
            },
            async move {
                wait.await.unwrap();
                Ok(())
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        assert_eq!(proof.await.unwrap(), (LockState::Uncertain, Ok(())));
    }

    #[tokio::test]
    async fn context_failure_still_drains_native() {
        let (release, wait) = std::sync::mpsc::channel();
        let (context_done, completed) = tokio::sync::oneshot::channel();
        let proof = tokio::spawn(context_with_native(
            move || {
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                LockState::Locked
            },
            async move {
                context_done.send(()).unwrap();
                Err::<(), _>(AdapterError)
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert!(!proof.is_finished());
        release.send(()).unwrap();
        assert_eq!(proof.await.unwrap(), (LockState::Locked, Err(AdapterError)));
    }

    #[test]
    fn foreground_or_lock_transition_invalidates_the_image_bracket() {
        let first = ("app.one".to_owned(), false);
        assert!(same_context(&first, "app.one", Some(false), &first));
        assert!(!same_context(&first, "app.two", Some(false), &first));
        assert!(!same_context(
            &first,
            "app.one",
            Some(false),
            &("app.two".into(), false)
        ));
        assert!(!same_context(&first, "app.one", Some(true), &first));
    }
}
