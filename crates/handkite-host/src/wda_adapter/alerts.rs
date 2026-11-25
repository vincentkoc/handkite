//! Explicit notification-alert authority. Never supplies ordinary input targets.
use super::focused_text::{id, rich_context, value};
use super::targets_pixels::Pixels;
use super::*;
use handkite_core::controller::{
    AlertButton, AlertCompletion, AlertDescriptor, AlertObservation, AlertRequest, PreparedText,
    RawAlertObservation,
};
use ring::rand::SecureRandom;
use serde_json::{Value, json};
mod context;
const CLASS: &str = "notification_permission_en_v1";
const ATTRS: &str = "type,enabled,displayed,rect,label,attribute/name,attribute/value";
const SOURCE: &str = OUTLINE_SOURCE;
const BUDGET: Duration = Duration::from_secs(5);
const HANDLE_AGE: Duration = Duration::from_secs(15);

struct PredicateStage {
    name: &'static str,
    span: Option<crate::timing::Span>,
    complete: bool,
}
impl PredicateStage {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            span: Some(crate::timing::Span::new(name)),
            complete: false,
        }
    }
    fn at(&mut self, name: &'static str) {
        crate::timing::outcome(self.name, true);
        self.span.take();
        self.name = name;
        self.span = Some(crate::timing::Span::new(name));
    }
    fn finish(&mut self) {
        crate::timing::outcome(self.name, true);
        self.span.take();
        self.complete = true;
    }
}
impl Drop for PredicateStage {
    fn drop(&mut self) {
        if !self.complete {
            crate::timing::outcome(self.name, false);
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Owner {
    bundle: String,
    pid: u64,
}
#[derive(Clone)]
struct Sample {
    owner: Owner,
    root: String,
    modal: Option<context::Modal>,
    alert: String,
    buttons: Vec<String>,
    display: Option<u64>,
    scale: f64,
    raw: RawObservation,
    started: Instant,
}
enum PostRead {
    Stable(Sample),
    Transition,
}
fn no_alert(tree: &context::Tree) -> bool {
    !tree.nodes.iter().any(|n| n.kind == "XCUIElementTypeAlert")
}
fn app_return(before: &(String, u64), after: &(String, u64)) -> bool {
    before.0 != after.0
        && ((before.0 == "com.apple.springboard") != (after.0 == "com.apple.springboard"))
}
fn read_dismissal(
    original: &Sample,
    until: Instant,
    mut read: impl FnMut(Instant) -> Result<PostRead, AdapterError>,
) -> Result<Sample, AdapterError> {
    for attempt in 0..2 {
        if Instant::now() >= until {
            return Err(AdapterError);
        }
        let result = read(until)?;
        if Instant::now() >= until {
            return Err(AdapterError);
        }
        match result {
            PostRead::Stable(sample) => {
                if attempt == 0 && sample.modal.is_some() && same(original, &sample) {
                    crate::timing::outcome("alert_post_read_retry", true);
                    continue;
                }
                return Ok(sample);
            }
            PostRead::Transition if attempt == 0 => {
                crate::timing::outcome("alert_post_read_retry", true);
            }
            PostRead::Transition => return Err(AdapterError),
        }
    }
    Err(AdapterError)
}
#[derive(Clone)]
struct Bound {
    key: cache::Key,
    sample: Sample,
    public: AlertDescriptor,
}
struct Input {
    bound: Bound,
    native: String,
    token: u64,
}
#[derive(Default)]
pub(super) struct State {
    captured: Option<Bound>,
    prepared: Option<Input>,
    sent: Option<Input>,
}
fn random(prefix: &str) -> Result<String, AdapterError> {
    let mut bytes = [0; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| AdapterError)?;
    Ok(format!(
        "{prefix}{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
async fn identity_mode(wda: &impl WdaRequest, sid: &str) -> Result<(), AdapterError> {
    let settings = value(
        wda,
        sid,
        "GET",
        &format!("/session/{sid}/appium/settings"),
        b"",
    )
    .await?;
    if settings["boundElementsByIndex"].as_bool() == Some(false) {
        Ok(())
    } else {
        Err(AdapterError)
    }
}
async fn owner(wda: &impl WdaRequest, sid: &str) -> Result<Owner, AdapterError> {
    let v = value(
        wda,
        sid,
        "GET",
        &format!("/session/{sid}/wda/activeAppInfo"),
        b"",
    )
    .await?;
    Ok(Owner {
        bundle: v["bundleId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(AdapterError)?
            .into(),
        pid: v["pid"].as_u64().filter(|n| *n > 0).ok_or(AdapterError)?,
    })
}
async fn unlocked(wda: &impl WdaRequest) -> Result<(), AdapterError> {
    let v = response_value(
        wda.request("GET", "/wda/locked", b"", BUDGET)
            .await
            .map_err(|_| AdapterError)?,
    )?;
    if v.as_bool() == Some(false) {
        Ok(())
    } else {
        Err(AdapterError)
    }
}
async fn find(
    wda: &impl WdaRequest,
    sid: &str,
    root: Option<&str>,
    kind: &str,
) -> Result<Vec<Value>, AdapterError> {
    let path = root.map_or_else(
        || format!("/session/{sid}/elements"),
        |r| format!("/session/{sid}/element/{r}/elements"),
    );
    let v = value(
        wda,
        sid,
        "POST",
        &path,
        &serde_json::to_vec(
            &json!({"using":"predicate string","value":format!("type == '{kind}'")}),
        )
        .map_err(|_| AdapterError)?,
    )
    .await?;
    let rows = v
        .as_array()
        .filter(|a| a.len() <= 64)
        .ok_or(AdapterError)?
        .clone();
    let mut seen = std::collections::BTreeSet::new();
    for row in &rows {
        if !seen.insert(id(row)?) {
            return Err(AdapterError);
        }
    }
    Ok(rows)
}
async fn no_system_alert(wda: &impl WdaRequest, sid: &str) -> Result<(), AdapterError> {
    let r = wda
        .request("GET", &format!("/session/{sid}/alert/text"), b"", BUDGET)
        .await
        .map_err(|_| AdapterError)?;
    let v: Value = serde_json::from_slice(&r.body).map_err(|_| AdapterError)?;
    if r.status == 404
        && v["sessionId"].as_str() == Some(sid)
        && v["value"]["error"].as_str() == Some("no such alert")
    {
        Ok(())
    } else {
        Err(AdapterError)
    }
}
async fn capture_attempt(
    wda: &impl WdaRequest,
    sid: &str,
    until: Instant,
    post: bool,
    original: Option<&Sample>,
) -> Result<Result<PostRead, AdapterError>, AdapterError> {
    let mut profile = PredicateStage::new("alert_response_profile");
    let captured = rich_context(
        wda,
        sid,
        ATTRS,
        before_input_deadline(until, async {
            let started = Instant::now();
            let begin = unix_ms()?;
            let mut stage = PredicateStage::new("alert_identity_before");
            identity_mode(wda, sid).await?;
            stage.at("alert_owner_before");
            let first = owner(wda, sid).await?;
            stage.at("alert_wda_lock_before");
            unlocked(wda).await?;
            stage.at("alert_display_before");
            let display = display::selection(wda, sid).await?;
            let scale = display::screen_scale(wda, display).await?;
            stage.at("alert_xml_before");
            let xml = value(wda, sid, "GET", &format!("/session/{sid}{SOURCE}"), b"")
                .await?
                .as_str()
                .ok_or(AdapterError)?
                .to_owned();
            stage.at("alert_context_before");
            let tree = context::scan(&xml)?;
            let viewport = context::rect(&tree.nodes[tree.app].attrs)?;
            if viewport[0] != 0. || viewport[1] != 0. {
                return Err(AdapterError);
            }
            let initial_owner = (first.bundle.clone(), first.pid);
            let tree_owner = context::owner(&tree)?;
            if tree_owner != initial_owner {
                if post && no_alert(&tree) && app_return(&initial_owner, &tree_owner) {
                    return Ok(PostRead::Transition);
                }
                return Err(AdapterError);
            }
            stage.at("alert_native_root");
            let roots = find(wda, sid, None, "XCUIElementTypeApplication").await?;
            if roots.len() != 1 || roots[0]["type"].as_str() != Some("XCUIElementTypeApplication") {
                return Err(AdapterError);
            }
            let root = id(&roots[0])?;
            stage.at("alert_native_alert");
            let alerts = find(wda, sid, Some(&root), "XCUIElementTypeAlert").await?;
            let (modal, alert, buttons) = if alerts.is_empty() && post {
                if tree.nodes.iter().any(|n| n.kind == "XCUIElementTypeAlert") {
                    return Err(AdapterError);
                }
                no_system_alert(wda, sid).await?;
                (None, String::new(), Vec::new())
            } else {
                if first.bundle != "com.apple.springboard" || alerts.len() != 1 {
                    return Err(AdapterError);
                }
                stage.at("alert_modal_before");
                let modal = context::semantic_modal(tree.clone(), first.pid)?;
                let alert = id(&alerts[0])?;
                if alert == root {
                    return Err(AdapterError);
                }
                if !context::matches(&alerts[0], &modal.tree.nodes[modal.alert]) {
                    return Err(AdapterError);
                }
                stage.at("alert_native_texts");
                let texts = find(wda, sid, Some(&alert), "XCUIElementTypeStaticText").await?;
                if texts.len() != modal.texts.len() {
                    return Err(AdapterError);
                }
                let mut text_ids = std::collections::BTreeSet::new();
                for idx in &modal.texts {
                    let matched = texts
                        .iter()
                        .filter(|r| context::matches_text(r, &modal.tree.nodes[*idx]))
                        .collect::<Vec<_>>();
                    if matched.len() != 1 {
                        return Err(AdapterError);
                    }
                    let native = id(matched[0])?;
                    if native == root || native == alert || !text_ids.insert(native) {
                        return Err(AdapterError);
                    }
                }
                stage.at("alert_native_buttons");
                let rows = find(wda, sid, Some(&alert), "XCUIElementTypeButton").await?;
                if rows.len() != modal.buttons.len() {
                    return Err(AdapterError);
                }
                let mut buttons = Vec::new();
                let mut used = std::collections::BTreeSet::new();
                for idx in &modal.buttons {
                    stage.at("alert_button_identity");
                    let candidates = rows
                        .iter()
                        .filter(|r| context::matches(r, &modal.tree.nodes[*idx]))
                        .collect::<Vec<_>>();
                    if candidates.len() != 1 {
                        return Err(AdapterError);
                    }
                    let native = id(candidates[0])?;
                    if native == root
                        || native == alert
                        || text_ids.contains(&native)
                        || !used.insert(native.clone())
                    {
                        return Err(AdapterError);
                    }
                    stage.at("alert_button_hittable");
                    let hit = value(
                        wda,
                        sid,
                        "GET",
                        &format!("/session/{sid}/element/{native}/attribute/hittable"),
                        b"",
                    )
                    .await?;
                    if hit != Value::Bool(true) && hit != Value::String("true".into()) {
                        return Err(AdapterError);
                    }
                    buttons.push(native);
                }
                (Some(modal), alert, buttons)
            };
            let same_original = original.is_some_and(|old| {
                old.owner == first
                    && old.root == root
                    && old.alert == alert
                    && old.buttons == buttons
                    && old
                        .modal
                        .as_ref()
                        .zip(modal.as_ref())
                        .is_some_and(|(a, b)| a.fingerprint == b.fingerprint)
            });
            stage.at("alert_image");
            let after = targets::screenshot(wda).await?;
            // A translucent alert can contain moving background pixels. Bracket
            // its native identity proof with complete semantic snapshots instead.
            stage.at("alert_xml_after");
            let final_xml = value(wda, sid, "GET", &format!("/session/{sid}{SOURCE}"), b"")
                .await?
                .as_str()
                .ok_or(AdapterError)?
                .to_owned();
            stage.at("alert_context_after");
            let final_tree = context::scan(&final_xml)?;
            if context::rect(&final_tree.nodes[final_tree.app].attrs)?
                != context::rect(&tree.nodes[tree.app].attrs)?
            {
                return Err(AdapterError);
            }
            let final_owner = context::owner(&final_tree)?;
            if final_owner != initial_owner {
                if post
                    && (no_alert(&tree) || same_original)
                    && no_alert(&final_tree)
                    && app_return(&initial_owner, &final_owner)
                {
                    return Ok(PostRead::Transition);
                }
                return Err(AdapterError);
            }
            if let Some(initial) = &modal {
                if post && no_alert(&final_tree) && same_original {
                    return Ok(PostRead::Transition);
                }
                let final_modal = context::semantic_modal(final_tree, first.pid)?;
                if final_modal.fingerprint != initial.fingerprint {
                    return Err(AdapterError);
                }
            } else {
                if final_tree
                    .nodes
                    .iter()
                    .any(|n| n.kind == "XCUIElementTypeAlert")
                    || !find(wda, sid, Some(&root), "XCUIElementTypeAlert")
                        .await?
                        .is_empty()
                {
                    return Err(AdapterError);
                }
                no_system_alert(wda, sid).await?;
            }
            stage.at("alert_owner_after");
            if owner(wda, sid).await? != first {
                return Err(AdapterError);
            }
            stage.at("alert_wda_lock_after");
            unlocked(wda).await?;
            stage.at("alert_display_after");
            display::verify_selection(wda, sid, display).await?;
            stage.at("alert_identity_after");
            identity_mode(wda, sid).await?;
            let viewport = context::rect(&tree.nodes[tree.app].attrs)?;
            stage.at("alert_window");
            let window =
                value(wda, sid, "GET", &format!("/session/{sid}/window/size"), b"").await?;
            let window = [
                window["width"].as_f64().ok_or(AdapterError)?,
                window["height"].as_f64().ok_or(AdapterError)?,
            ];
            stage.at("alert_image_mapping");
            let b = Pixels::decode(&after.bytes)?;
            b.validate_mapping(viewport, scale, window)?;
            stage.finish();
            Ok(PostRead::Stable(Sample {
                owner: first.clone(),
                root,
                modal,
                alert,
                buttons,
                display,
                scale,
                started,
                raw: RawObservation {
                    connected_usb: HardwareId::new("unbound").map_err(|_| AdapterError)?,
                    wda_target: HardwareId::new("unbound").map_err(|_| AdapterError)?,
                    window: ObservationWindow {
                        started_unix_ms: begin,
                        finished_unix_ms: unix_ms()?,
                    },
                    state: DeviceState {
                        lock: LockState::Unlocked,
                        wda_ready: true,
                        foreground_app: Some(first.bundle),
                    },
                    accessibility_tree: String::new(),
                    screenshot: after,
                },
            }))
        }),
    )
    .await?;
    profile.finish();
    let mut restored = PredicateStage::new("alert_restored_identity");
    identity_mode(wda, sid).await?;
    restored.finish();
    Ok(captured)
}

#[cfg(test)]
async fn capture(
    wda: &impl WdaRequest,
    sid: &str,
    until: Instant,
    post: bool,
) -> Result<Result<Sample, AdapterError>, AdapterError> {
    Ok(capture_attempt(wda, sid, until, post, None)
        .await?
        .and_then(|read| match read {
            PostRead::Stable(sample) => Ok(sample),
            PostRead::Transition => Err(AdapterError),
        }))
}

async fn click_once(
    wda: &impl WdaRequest,
    sid: &str,
    native: &str,
) -> Result<TransportReceipt, AdapterError> {
    value(
        wda,
        sid,
        "POST",
        &format!("/session/{sid}/element/{native}/click"),
        b"{}",
    )
    .await?;
    Ok(TransportReceipt {
        accepted: Some(true),
        action_kind: "tap_alert".into(),
    })
}
fn same(before: &Sample, after: &Sample) -> bool {
    before.owner == after.owner
        && before.root == after.root
        && before.alert == after.alert
        && before.buttons == after.buttons
        && before.display == after.display
        && before.scale == after.scale
        && before
            .modal
            .as_ref()
            .zip(after.modal.as_ref())
            .is_some_and(|(a, b)| a.fingerprint == b.fingerprint && a.viewport == b.viewport)
}
pub(super) fn supported_notification(xml: &str) -> bool {
    context::scan(xml)
        .and_then(|t| {
            let (_, pid) = context::owner(&t)?;
            context::modal(t, pid)
        })
        .is_ok()
}
impl WdaAdapter {
    fn alert_capture(&mut self, until: Instant, post: bool) -> Result<Sample, AdapterError> {
        match self.alert_read(until, post, None)? {
            PostRead::Stable(sample) => Ok(sample),
            PostRead::Transition => Err(AdapterError),
        }
    }
    fn alert_read(
        &mut self,
        until: Instant,
        post: bool,
        original: Option<&Sample>,
    ) -> Result<PostRead, AdapterError> {
        let started = Instant::now();
        let begin = unix_ms()?;
        self.validate_owned_with_deadline(Some(until))?;
        let key = self.cache_key().ok_or(AdapterError)?;
        let device = key.device.clone();
        let native_started = Instant::now();
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?));
        let (native, result) =
            self.runtime
                .as_ref()
                .ok_or(AdapterError)?
                .block_on(targets::capture_with_native(
                    move || native_lock::query_until(&device, until).state,
                    capture_attempt(&wda, &key.session, until, post, original),
                ));
        self.cache
            .refresh_native(Some(key.clone()), native_started, || native);
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.text_response_poisoned = true;
                return Err(e);
            }
        };
        crate::timing::outcome("alert_native_before", native == LockState::Unlocked);
        if native != LockState::Unlocked {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        let mut result = result?;
        let mut after_stage = PredicateStage::new("alert_native_after");
        if self.native_fresh(Some(until)) != LockState::Unlocked {
            self.error_diagnostic = Some(AdapterDiagnostic::NativeBefore);
            return Err(AdapterError);
        }
        after_stage.at("alert_ownership_after");
        self.validate_owned_with_deadline(Some(until))?;
        after_stage.at("alert_deadline_and_session");
        if self.cache_key().as_ref() != Some(&key) || Instant::now() >= until {
            return Err(AdapterError);
        }
        if let PostRead::Stable(ref mut sample) = result {
            sample.raw.connected_usb = key.device.clone();
            sample.raw.wda_target = key.device;
            sample.raw.window = ObservationWindow {
                started_unix_ms: begin,
                finished_unix_ms: unix_ms()?,
            };
            sample.started = started;
        }
        after_stage.finish();
        Ok(result)
    }
    pub(super) fn observe_notification(&mut self) -> Result<RawAlertObservation, AdapterError> {
        self.alert_state = State::default();
        self.targets = None;
        self.target_snapshot = None;
        self.target_delta = None;
        self.target_input = None;
        self.target_readback = None;
        self.home_readback_proof = None;
        self.cache.clear_full();
        self.finish_pending_effect(true)?;
        self.error_diagnostic = Some(AdapterDiagnostic::AlertCapture);
        let sample = self.alert_capture(Instant::now() + BUDGET, false)?;
        self.error_diagnostic = Some(AdapterDiagnostic::AlertUnsupported);
        let modal = sample.modal.as_ref().ok_or(AdapterError)?;
        let buttons = modal
            .buttons
            .iter()
            .map(|i| {
                Ok(AlertButton {
                    target_id: random("ab_")?,
                    label: modal.tree.nodes[*i].attrs["label"].clone(),
                    rect: context::rect(&modal.tree.nodes[*i].attrs)?,
                })
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;
        let public = AlertDescriptor {
            alert_id: random("a_")?,
            class: CLASS.into(),
            title: modal.title.clone(),
            body: modal.body.clone(),
            buttons,
        };
        let raw = RawAlertObservation {
            observation: sample.raw.clone(),
            alert: public.clone(),
        };
        self.alert_state.captured = Some(Bound {
            key: self.cache_key().ok_or(AdapterError)?,
            sample,
            public,
        });
        self.error_diagnostic = None;
        Ok(raw)
    }
    pub(super) fn prepare_notification(
        &mut self,
        request: &AlertRequest,
        baseline: &AlertObservation,
    ) -> Result<PreparedText, AdapterError> {
        self.error_diagnostic = Some(AdapterDiagnostic::AlertRevalidation);
        let bound = self.alert_state.captured.take().ok_or(AdapterError)?;
        if bound.public != baseline.alert
            || request.alert_id != bound.public.alert_id
            || self.cache_key().as_ref() != Some(&bound.key)
        {
            return Err(AdapterError);
        }
        let index = bound
            .public
            .buttons
            .iter()
            .position(|b| b.target_id == request.target_id)
            .ok_or(AdapterError)?;
        let native = bound.sample.buttons[index].clone();
        let until = bound.sample.started + HANDLE_AGE;
        if Instant::now() >= until {
            return Err(AdapterError);
        }
        self.text_token = self.text_token.checked_add(1).ok_or(AdapterError)?;
        let proof = PreparedText {
            token: self.text_token,
            connected_usb: self.expected.clone(),
            wda_target: self.expected.clone(),
            window: baseline.observation.window.clone(),
            state: baseline.observation.state.clone(),
            valid_until: until,
        };
        self.alert_state.prepared = Some(Input {
            bound,
            native,
            token: self.text_token,
        });
        self.error_diagnostic = None;
        Ok(proof)
    }
    pub(super) fn send_notification(
        &mut self,
        token: u64,
        deadline: Instant,
    ) -> Result<TransportReceipt, AdapterError> {
        self.error_diagnostic = Some(AdapterDiagnostic::AlertRevalidation);
        let input = self.alert_state.prepared.take().ok_or(AdapterError)?;
        if self.cache_key().as_ref() != Some(&input.bound.key)
            || input.token != token
            || Instant::now() >= deadline
            || deadline > input.bound.sample.started + HANDLE_AGE
        {
            return Err(AdapterError);
        }
        let fresh = self.alert_capture(deadline, false)?;
        if self.cache_key().as_ref() != Some(&input.bound.key) || !same(&input.bound.sample, &fresh)
        {
            return Err(AdapterError);
        }
        let key = self.cache_key().ok_or(AdapterError)?;
        self.validate_owned_with_deadline(Some(deadline))?;
        let runner = self.owned_runner.as_ref().ok_or(AdapterError)?;
        let wda = UsbWda::new(self.usb.clone())
            .with_write_admission(Some(runner.write_admission().ok_or(AdapterError)?))
            .with_write_deadline(deadline);
        self.runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(before_input_deadline(
                deadline,
                identity_mode(&wda, &key.session),
            ))?;
        // Past this boundary delivery is unknown on every transport/protocol error.
        self.error_diagnostic = None;
        self.runtime
            .as_ref()
            .ok_or(AdapterError)?
            .block_on(click_once(&wda, &key.session, &input.native))?;
        self.alert_state.sent = Some(input);
        if self.validate_owned().is_err() {
            self.error_diagnostic = Some(AdapterDiagnostic::OwnershipAfterAck);
            self.alert_state.sent = None;
        }
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: "tap_alert".into(),
        })
    }
    pub(super) fn finish_notification(
        &mut self,
        token: u64,
    ) -> Result<AlertCompletion, AdapterError> {
        let input = self
            .alert_state
            .sent
            .take()
            .filter(|i| i.token == token)
            .ok_or(AdapterError)?;
        if self.cache_key().as_ref() != Some(&input.bound.key) {
            return Err(AdapterError);
        }
        let until = Instant::now() + BUDGET;
        let sample = read_dismissal(&input.bound.sample, until, |until| {
            if self.cache_key().as_ref() != Some(&input.bound.key) {
                return Err(AdapterError);
            }
            self.alert_read(until, true, Some(&input.bound.sample))
        })?;
        // A replacement alert is never a confirmed dismissal.
        let dismissed = sample.modal.is_none();
        let raw = sample.raw;
        drop(input);
        Ok(AlertCompletion {
            image: RawImageObservation {
                connected_usb: raw.connected_usb,
                wda_target: raw.wda_target,
                window: raw.window,
                screenshot: raw.screenshot,
            },
            dismissed,
        })
    }
}

#[cfg(test)]
pub(super) fn test_fixture() -> String {
    context::fixture()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        case: u8,
        settings: RefCell<Value>,
        screenshots: Cell<usize>,
        sources: Cell<usize>,
        owners: Cell<usize>,
        clicks: Cell<usize>,
    }
    impl Fake {
        fn new(case: u8) -> Self {
            Self {
                case,
                settings: RefCell::new(
                    json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"type","boundElementsByIndex":false}),
                ),
                screenshots: Cell::new(0),
                sources: Cell::new(0),
                owners: Cell::new(0),
                clicks: Cell::new(0),
            }
        }
    }
    fn row(native: &str, kind: &str, label: &str, r: [f64; 4]) -> Value {
        json!({"ELEMENT":native,"type":format!("XCUIElementType{kind}"),"enabled":true,"displayed":true,"label":label,"attribute/name":label,"attribute/value":"","rect":{"x":r[0],"y":r[1],"width":r[2],"height":r[3]}})
    }
    fn image(change: u8) -> String {
        let mut rgba = vec![0u8; 100 * 200 * 4];
        if change == 1 {
            rgba[0] = 1;
        }
        if change == 2 {
            rgba[(100 * 80 + 30) * 4] = 1;
        }
        let mut bytes = vec![];
        {
            let mut e = png::Encoder::new(&mut bytes, 100, 200);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&rgba).unwrap();
        }
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
    impl WdaRequest for Fake {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            let mut status = 200;
            let v = if path.ends_with("/appium/settings") {
                if method == "POST" {
                    let p: Value = serde_json::from_slice(body).unwrap();
                    for (k, v) in p["settings"].as_object().unwrap() {
                        self.settings.borrow_mut()[k] = v.clone();
                    }
                }
                let mut v = self.settings.borrow().clone();
                if self.case == 12 {
                    v["boundElementsByIndex"] = json!(true);
                }
                if self.case == 13 {
                    v.as_object_mut().unwrap().remove("boundElementsByIndex");
                }
                v
            } else if path.ends_with("/wda/activeAppInfo") {
                let n = self.owners.get();
                self.owners.set(n + 1);
                if self.case == 30 {
                    json!({"bundleId":"com.example.returned","pid":99})
                } else {
                    json!({"bundleId":if self.case==1{"com.example.lookalike"}else{"com.apple.springboard"},"pid":if self.case==16{0}else if self.case==2&&n>0{43}else{42}})
                }
            } else if path == "/wda/locked" {
                json!(self.case == 3)
            } else if path == "/wda/screen" {
                json!({"scale":1})
            } else if path == "/screenshot" {
                let n = self.screenshots.get();
                self.screenshots.set(n + 1);
                json!(image(if matches!(self.case, 4 | 11) {
                    1
                } else if self.case == 5 {
                    2
                } else {
                    0
                }))
            } else if path.contains("/source?") {
                assert_eq!(path, format!("/session/test{OUTLINE_SOURCE}"));
                let source = self.sources.get();
                self.sources.set(source + 1);
                if source > 0 && self.case == 21 {
                    return Err(crate::wda_transport::WdaError::NotConnected);
                }
                let mut xml = context::fixture();
                if self.case == 1 {
                    xml = xml.replace("com.apple.springboard", "com.example.lookalike");
                }
                if self.case == 6 {
                    xml = xml.replace("processId=\"42\"", "processId=\"43\"");
                }
                if self.case == 7 {
                    xml=xml.replace("</XCUIElementTypeApplication>","<XCUIElementTypeSecureTextField visible='false'/></XCUIElementTypeApplication>");
                }
                if matches!(self.case, 10 | 11 | 23 | 24 | 28 | 29 | 30 | 32 | 33)
                    || (matches!(self.case, 27 | 31) && source > 0)
                {
                    let from = xml.find("<XCUIElementTypeAlert").unwrap();
                    let to = xml.find("</XCUIElementTypeAlert>").unwrap()
                        + "</XCUIElementTypeAlert>".len();
                    xml.replace_range(from..to, "");
                }
                if self.case == 33 || (self.case == 32 && source > 0) {
                    xml = xml.replace("processId=\"42\"", "processId=\"43\"");
                }
                if self.case == 29
                    || self.case == 30
                    || (matches!(self.case, 28 | 31) && source > 0)
                {
                    xml = xml
                        .replace("com.apple.springboard", "com.example.returned")
                        .replace("processId=\"42\"", "processId=\"99\"");
                }
                if source > 0 {
                    match self.case {
                        18 => xml = xml.replace("“Fixture”", "“Replacement”"),
                        19 => xml = xml.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeSecureTextField visible='false'/></XCUIElementTypeApplication>"),
                        20 => xml = xml.replace("width=\"30\"", "width=\"29\""),
                        22 => xml = xml.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeSheet visible='false'/></XCUIElementTypeApplication>"),
                        23 => xml = context::fixture(),
                        24 => xml = xml.replace("</XCUIElementTypeApplication>", "<XCUIElementTypeSecureTextField visible='false'/></XCUIElementTypeApplication>"),
                        _ => {}
                    }
                }
                json!(xml.replace(" visible=\"true\"", ""))
            } else if path.ends_with("/window/size") {
                json!({"width":100,"height":200})
            } else if path.ends_with("/attribute/hittable") {
                if self.case == 17 {
                    Value::Null
                } else {
                    json!(self.case != 8)
                }
            } else if path.ends_with("/elements") {
                let query: Value = serde_json::from_slice(body).unwrap();
                if path.contains("/element/root/")
                    && matches!(self.case, 10 | 11 | 23 | 24 | 28 | 29 | 30 | 32 | 33)
                {
                    json!([])
                } else if path.contains("/element/root/") {
                    json!([row(
                        if self.case == 15 { "root" } else { "alert" },
                        "Alert",
                        "notification",
                        [10., 40., 80., 120.]
                    )])
                } else if path.contains("/element/alert/")
                    && query["value"].as_str().unwrap().contains("StaticText")
                {
                    let mut title = row(
                        "title",
                        "StaticText",
                        "“Fixture” Would Like to Send You Notifications",
                        [15., 45., 70., 25.],
                    );
                    if self.case == 25 {
                        title["displayed"] = json!(false);
                    }
                    json!([
                        title,
                        row("body", "StaticText", context::BODY, [15., 75., 70., 50.])
                    ])
                } else if path.contains("/element/alert/") {
                    let mut rows = vec![
                        row("deny", "Button", "Don't Allow", [15., 140., 30., 15.]),
                        row(
                            if self.case == 9 { "deny" } else { "allow" },
                            "Button",
                            "Allow",
                            [55., 140., 30., 15.],
                        ),
                    ];
                    if self.case == 26 {
                        rows[0]["displayed"] = json!(false);
                    }
                    json!(rows)
                } else {
                    json!([row(
                        "root",
                        "Application",
                        "SpringBoard",
                        [0., 0., 100., 200.]
                    )])
                }
            } else if path.ends_with("/alert/text") {
                status = 404;
                json!({"error":"no such alert"})
            } else if path.ends_with("/click") {
                self.clicks.set(self.clicks.get() + 1);
                if self.case == 14 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                json!(null)
            } else {
                panic!("unexpected fake request {method} {path}")
            };
            Ok(HttpResponse {
                status,
                body: serde_json::to_vec(&json!({"sessionId":"test","value":v})).unwrap(),
            })
        }
    }
    #[tokio::test]
    async fn full_native_owner_hierarchy_and_hit_proof_are_required_without_any_click() {
        for case in [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 15, 16, 17, 18, 19, 20, 21, 22, 25, 26,
        ] {
            let fake = Fake::new(case);
            let result = capture(&fake, "test", Instant::now() + BUDGET, false).await;
            if matches!(case, 0 | 4 | 5) {
                assert!(result.unwrap().is_ok(), "case {case}");
                assert_eq!(fake.sources.get(), 2);
            } else {
                assert!(result.is_err() || result.unwrap().is_err(), "case {case}");
            }
            assert_eq!(fake.clicks.get(), 0);
            assert_eq!(
                *fake.settings.borrow(),
                json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"type","boundElementsByIndex":false})
            );
        }
    }
    #[tokio::test]
    async fn revalidation_compares_native_identity_geometry_and_complete_context() {
        let a = capture(&Fake::new(0), "test", Instant::now() + BUDGET, false)
            .await
            .unwrap()
            .unwrap();
        assert!(same(&a, &a));
        for case in 0..7 {
            let mut b = a.clone();
            match case {
                0 => b.owner.pid += 1,
                1 => b.root = "newroot".into(),
                2 => b.alert = "newalert".into(),
                3 => b.buttons.swap(0, 1),
                4 => b.display = Some(4),
                5 => b.scale = 2.,
                _ => b.modal.as_mut().unwrap().fingerprint[0]
                    .attrs
                    .insert("label".into(), "changed".into())
                    .map(|_| ())
                    .unwrap_or(()),
            };
            assert!(!same(&a, &b));
        }
        let outside = capture(&Fake::new(4), "test", Instant::now() + BUDGET, false)
            .await
            .unwrap()
            .unwrap();
        assert!(same(&a, &outside));
        let translucent = capture(&Fake::new(5), "test", Instant::now() + BUDGET, false)
            .await
            .unwrap()
            .unwrap();
        assert!(same(&a, &translucent));
    }
    #[tokio::test]
    async fn expired_capture_never_dispatches_or_leaves_response_settings_changed() {
        let fake = Fake::new(0);
        assert!(
            capture(
                &fake,
                "test",
                Instant::now() - Duration::from_secs(1),
                false
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(fake.clicks.get(), 0);
        assert_eq!(
            *fake.settings.borrow(),
            json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"type","boundElementsByIndex":false})
        );
    }
    #[tokio::test]
    async fn postcondition_requires_complete_semantic_absence_despite_background_motion() {
        let present = capture(&Fake::new(0), "test", Instant::now() + BUDGET, true)
            .await
            .unwrap()
            .unwrap();
        assert!(present.modal.is_some());
        let absent = capture(&Fake::new(10), "test", Instant::now() + BUDGET, true)
            .await
            .unwrap()
            .unwrap();
        assert!(absent.modal.is_none());
        let changing = Fake::new(11);
        assert!(
            capture(&changing, "test", Instant::now() + BUDGET, true)
                .await
                .unwrap()
                .unwrap()
                .modal
                .is_none()
        );
        assert_eq!(changing.clicks.get(), 0);
        for case in [23, 24] {
            let replaced = Fake::new(case);
            assert!(
                capture(&replaced, "test", Instant::now() + BUDGET, true)
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(replaced.clicks.get(), 0);
        }
    }
    #[tokio::test]
    async fn late_acknowledgement_is_retained_without_a_second_click() {
        let fake = Fake::new(14);
        let receipt = click_once(&fake, "test", "deny").await.unwrap();
        assert_eq!(receipt.accepted, Some(true));
        assert_eq!(receipt.action_kind, "tap_alert");
        assert_eq!(fake.clicks.get(), 1);
    }
    #[test]
    fn dismissal_retries_mid_animation_and_app_return_without_resending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let original = rt
            .block_on(capture(
                &Fake::new(0),
                "test",
                Instant::now() + BUDGET,
                false,
            ))
            .unwrap()
            .unwrap();
        for first in [0, 27, 28, 29, 31] {
            let sent = Fake::new(0);
            rt.block_on(click_once(&sent, "test", "deny")).unwrap();
            let until = Instant::now() + BUDGET;
            let mut calls = 0;
            let sample = read_dismissal(&original, until, |deadline| {
                assert_eq!(deadline, until);
                let fixture = Fake::new(if calls == 0 { first } else { 30 });
                calls += 1;
                rt.block_on(capture_attempt(
                    &fixture,
                    "test",
                    deadline,
                    true,
                    Some(&original),
                ))?
            })
            .unwrap();
            assert_eq!(calls, 2);
            assert!(sample.modal.is_none());
            assert_eq!(sent.clicks.get(), 1);
        }
    }
    #[test]
    fn dismissal_never_retries_a_hard_failure_or_replacement() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let original = rt
            .block_on(capture(
                &Fake::new(0),
                "test",
                Instant::now() + BUDGET,
                false,
            ))
            .unwrap()
            .unwrap();
        for case in [1, 3, 6, 7, 12, 13, 19, 21, 23, 24, 32, 33] {
            let mut calls = 0;
            let result = read_dismissal(&original, Instant::now() + BUDGET, |until| {
                calls += 1;
                rt.block_on(capture_attempt(
                    &Fake::new(case),
                    "test",
                    until,
                    true,
                    Some(&original),
                ))?
            });
            assert!(result.is_err(), "case {case}");
            assert_eq!(calls, 1, "case {case}");
        }
        let mut replacement = original.clone();
        replacement.alert = "replacement".into();
        let mut calls = 0;
        let sample = read_dismissal(&original, Instant::now() + BUDGET, |_| {
            calls += 1;
            Ok(PostRead::Stable(replacement.clone()))
        })
        .unwrap();
        assert!(sample.modal.is_some());
        assert_eq!(calls, 1);
    }
    #[test]
    fn dismissal_retry_is_limited_to_two_reads_and_one_deadline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let original = rt
            .block_on(capture(
                &Fake::new(0),
                "test",
                Instant::now() + BUDGET,
                false,
            ))
            .unwrap()
            .unwrap();
        let mut calls = 0;
        assert!(
            read_dismissal(&original, Instant::now() + BUDGET, |_| {
                calls += 1;
                Ok(PostRead::Transition)
            })
            .is_err()
        );
        assert_eq!(calls, 2);
        assert!(read_dismissal(&original, Instant::now(), |_| panic!("expired read")).is_err());
        let until = Instant::now() + Duration::from_millis(1);
        let mut calls = 0;
        assert!(
            read_dismissal(&original, until, |deadline| {
                assert_eq!(deadline, until);
                calls += 1;
                std::thread::sleep(Duration::from_millis(3));
                Ok(PostRead::Transition)
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }
}
