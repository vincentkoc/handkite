use super::*;

impl WdaAdapter {
    pub(super) fn navigation_frame(&mut self) -> Result<RawObservation, AdapterError> {
        self.error_diagnostic = None;
        self.cache.clear_full();
        let started_unix_ms = unix_ms()?;
        self.validate_owned()?;
        if self.owned_session.is_none() || self.owned_runner.is_none() {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let session = self.owned_session.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone());
        let (native_before, before) = {
            let _timing = crate::timing::Span::new("navigation_before_proof");
            self.navigation_context(&wda, session)?
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
        let (native_after, after) = {
            let _timing = crate::timing::Span::new("navigation_after_proof");
            self.navigation_context(&wda, session)?
        };
        let after = after?;
        let coherent = same_context(&before, &frame.app, frame.reported, &after);
        crate::timing::outcome("navigation_context_coherent", coherent);
        if !coherent {
            return Err(AdapterError);
        }
        let unlocked = native_before == LockState::Unlocked && frame.reported == Some(false);
        let locked = native_before == LockState::Locked && frame.reported == Some(true);
        if !unlocked && !locked {
            return Err(AdapterError);
        }
        if unlocked && frame.app.is_empty() {
            return Err(AdapterError);
        }
        if native_after != native_before {
            return Err(AdapterError);
        }
        self.validate_owned()?;
        Ok(RawObservation {
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            window: ObservationWindow {
                started_unix_ms,
                finished_unix_ms: unix_ms()?,
            },
            state: DeviceState {
                lock: native_before,
                wda_ready: true,
                foreground_app: Some(frame.app),
            },
            accessibility_tree: String::new(),
            screenshot: frame.screenshot,
        })
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
        let wda = UsbWda::new(self.usb.clone()).with_write_deadline(deadline);
        let session = self.owned_session.as_ref().ok_or(AdapterError)?;
        self.runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(before_input_deadline(
                deadline,
                focused_text::app_matches(&wda, session, app),
            ))?;
        self.execute_with_admission(action, deadline, Some(admission))
    }
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
#[cfg(test)]
mod tests {
    use super::*;

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
