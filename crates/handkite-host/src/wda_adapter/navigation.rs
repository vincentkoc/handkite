use super::*;

impl WdaAdapter {
    pub(super) fn navigation_frame(&mut self) -> Result<RawObservation, AdapterError> {
        self.error_diagnostic = None;
        self.cache.clear_full();
        let started_unix_ms = unix_ms()?;
        self.validate_owned()?;
        let native_before = self.native_fresh(None);
        if self.owned_session.is_none()
            || self.owned_runner.is_none()
            || native_before == LockState::Uncertain
        {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let session = self.owned_session.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone());
        let before = self
            .runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(context(&wda, session))?;
        let frame = self.capture_frame(None)?;
        let after = self
            .runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(context(&wda, session))?;
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
        if self.native_fresh(None) != native_before {
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
