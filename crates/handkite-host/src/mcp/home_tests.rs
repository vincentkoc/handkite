use super::*;
use std::collections::VecDeque;

fn observation(lock: &str) -> Value {
    json!({"device":"phone","revision":8,
        "window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"foreground_app":"com.apple.springboard","lock":lock,"wda_ready":true},
        "accessibility_tree":"","screenshot_format":"png","screenshot_base64":"AQ=="})
}

fn observe(targets: bool, replies: Vec<Value>) -> (Value, Vec<String>, Session) {
    let mut session = Session::default();
    session.target_inventory = Some((
        handkite_core::protocol::Revision(3),
        serde_json::from_value(observation("unlocked")["state"].clone()).unwrap(),
        vec![],
    ));
    let mut methods = vec![];
    let mut replies = VecDeque::from(replies);
    let result = tool_result(
        Tool::Observe,
        presentation::Options {
            targets,
            tree_format: presentation::TreeFormat::Elements,
            ..Default::default()
        },
        &DeviceKey::new("phone").unwrap(),
        &mut session,
        &mut |bytes| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            methods.push(request["method"].as_str().unwrap().to_owned());
            Ok(
                serde_json::to_vec(&replies.pop_front().expect("unexpected extra request"))
                    .unwrap(),
            )
        },
    );
    assert!(replies.is_empty());
    (result, methods, session)
}

#[test]
fn home_target_failure_returns_fresh_navigation_without_target_authority() {
    let (result, methods, session) = observe(
        true,
        vec![
            json!({"type":"error","code":"AdapterTargetHome"}),
            json!({"type":"observed_navigation","observation":observation("unlocked")}),
        ],
    );
    assert_eq!(methods, ["observe_targets", "observe_navigation"]);
    let p = &result["structuredContent"];
    assert_eq!(p["revision_scope"], "navigation");
    assert_eq!(p["tree"]["reason"], "home_navigation_only");
    assert_eq!(p["target_actionable"], false);
    assert_eq!(p["metadata"]["state"]["lock"], "unlocked");
    assert!(p.get("targets").is_none());
    assert!(session.target_inventory.is_none());
}

#[test]
fn uncertain_home_tree_cannot_replace_confirmed_navigation_state() {
    let mut full = observation("uncertain");
    full["accessibility_tree"] = json!("<home />");
    let (result, methods, _) = observe(
        false,
        vec![
            json!({"type":"observed","observation":full}),
            json!({"type":"observed_navigation","observation":observation("unlocked")}),
        ],
    );
    assert_eq!(methods, ["observe", "observe_navigation"]);
    assert_eq!(result["structuredContent"]["revision_scope"], "navigation");
    assert_eq!(
        result["structuredContent"]["metadata"]["state"]["lock"],
        "unlocked"
    );
}

#[test]
fn unrelated_capture_failure_does_not_take_home_fallback() {
    for code in [
        "AdapterTargetCapture",
        "AdapterTargetSettings",
        "AdapterTargetOcr",
        "AdapterNativeBefore",
    ] {
        let (result, methods, _) = observe(true, vec![json!({"type":"error","code":code})]);
        assert_eq!(methods, ["observe_targets"]);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["code"], code);
    }
}

#[test]
fn home_fallback_preserves_failure_identity_and_lock_evidence() {
    let mut wrong_device = observation("unlocked");
    wrong_device["device"] = json!("other");
    for reply in [
        json!({"type":"error","code":"AdapterNativeBefore"}),
        json!({"type":"observed_navigation","observation":wrong_device}),
    ] {
        let (result, methods, session) = observe(
            true,
            vec![json!({"type":"error","code":"AdapterTargetHome"}), reply],
        );
        assert_eq!(methods, ["observe_targets", "observe_navigation"]);
        assert_eq!(result["isError"], true);
        assert!(session.target_inventory.is_none());
    }
    let (result, _, session) = observe(
        true,
        vec![
            json!({"type":"error","code":"AdapterTargetHome"}),
            json!({"type":"observed_navigation","observation":observation("locked")}),
        ],
    );
    assert_eq!(
        result["structuredContent"]["metadata"]["state"]["lock"],
        "locked"
    );
    assert_eq!(result["structuredContent"]["target_actionable"], false);
    assert!(session.target_inventory.is_none());
}

#[test]
fn wrong_device_home_tree_never_reaches_fallback() {
    let mut home = observation("uncertain");
    home["device"] = json!("other");
    let (result, methods, _) = observe(false, vec![json!({"type":"observed","observation":home})]);
    assert_eq!(methods, ["observe"]);
    assert_eq!(result["structuredContent"]["code"], "DeviceMismatch");
}
