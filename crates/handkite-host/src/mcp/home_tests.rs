use super::*;
use std::collections::VecDeque;

fn observation(lock: &str) -> Value {
    json!({"device":"phone","revision":8,
        "window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"foreground_app":"com.apple.springboard","lock":lock,"wda_ready":true},
        "accessibility_tree":"","screenshot_format":"png","screenshot_base64":"AQ=="})
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}})
}

#[test]
fn home_observe_is_fast_then_launch_needs_only_one_app_observe() {
    let mut session = Session::default();
    session.acquire(&mut |_| {
        Ok(serde_json::to_vec(&json!({
            "type":"acquired","lease":"0123456789abcdef0123456789abcdef"
        }))
        .unwrap())
    });
    let mut app = observation("unlocked");
    app["state"]["foreground_app"] = json!("example.app");
    app["accessibility_tree"] = json!("<Application/>");
    let mut replies = VecDeque::from([
        json!({"type":"navigated","receipt":{"accepted":true,"action_kind":"home"},
            "effect":"changed","after":observation("unlocked")}),
        json!({"type":"observed_navigation","observation":observation("unlocked")}),
        json!({"type":"navigated","receipt":{"accepted":true,"action_kind":"launch_app"},
            "effect":"changed","after":app}),
        json!({"type":"observed","observation":app}),
    ]);
    let mut methods = vec![];
    for request in [
        tool_call("handkite_home", json!({"expected_revision":8})),
        tool_call("handkite_observe", json!({"image":"omit"})),
        tool_call(
            "handkite_launch_app",
            json!({"expected_revision":8,"bundle_id":"example.app"}),
        ),
        tool_call("handkite_observe", json!({"image":"omit"})),
    ] {
        let reply = process(
            &request,
            &DeviceKey::new("phone").unwrap(),
            Capabilities::ObservationNavigationInteraction,
            &mut session,
            &mut |bytes| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                Ok(serde_json::to_vec(&replies.pop_front().expect("extra read")).unwrap())
            },
        )
        .unwrap();
        assert_ne!(reply["result"]["isError"], true);
    }
    assert_eq!(
        methods,
        ["navigate", "observe_navigation", "navigate", "observe"]
    );
    assert!(!session.home_observe_hint);
}

#[test]
fn every_other_tool_clears_home_hint_even_when_it_refuses() {
    for name in [
        "handkite_acquire_control",
        "handkite_release_control",
        "handkite_home",
        "handkite_launch_clock",
        "handkite_launch_app",
        "handkite_tap",
        "handkite_tap_element",
        "handkite_text_element",
        "handkite_click_button",
        "handkite_double_tap",
        "handkite_long_press",
        "handkite_swipe",
        "handkite_text",
        "handkite_key",
    ] {
        let mut session = Session::default();
        session.home_observe_hint = true;
        process(
            &tool_call(name, json!({})),
            &DeviceKey::new("phone").unwrap(),
            Capabilities::ObservationNavigationInteraction,
            &mut session,
            &mut |_| Ok(serde_json::to_vec(&json!({"type":"error","code":"Busy"})).unwrap()),
        );
        assert!(!session.home_observe_hint, "{name}");
    }
}

#[test]
fn only_confirmed_home_reply_seeds_home_hint() {
    let mut wrong_device = observation("unlocked");
    wrong_device["device"] = json!("other");
    let mut wrong_app = observation("unlocked");
    wrong_app["state"]["foreground_app"] = json!("example.app");
    for (after, accepted, effect, expected) in [
        (observation("unlocked"), Some(true), "changed", true),
        (observation("unlocked"), Some(true), "pending", false),
        (observation("locked"), Some(true), "changed", false),
        (observation("uncertain"), Some(true), "changed", false),
        (wrong_device, Some(true), "changed", false),
        (wrong_app, Some(true), "changed", false),
        (Value::Null, Some(true), "changed", false),
        (observation("unlocked"), None, "changed", false),
        (observation("unlocked"), Some(true), "unconfirmed", false),
    ] {
        let mut session = Session::default();
        session.acquire(&mut |_| {
            Ok(serde_json::to_vec(&json!({
                "type":"acquired","lease":"0123456789abcdef0123456789abcdef"
            }))
            .unwrap())
        });
        process(
            &tool_call("handkite_home", json!({"expected_revision":8})),
            &DeviceKey::new("phone").unwrap(),
            Capabilities::ObservationNavigationInteraction,
            &mut session,
            &mut |_| {
                Ok(serde_json::to_vec(&json!({
                    "type":"navigated","receipt":{"accepted":accepted,"action_kind":"home"},
                    "effect":effect,"after":after
                }))
                .unwrap())
            },
        );
        assert_eq!(session.home_observe_hint, expected, "{effect}: {after}");
    }
}

#[test]
fn known_home_uses_one_fresh_navigation_read_and_honors_image_omit() {
    for targets in [false, true] {
        let mut session = Session::default();
        let device = DeviceKey::new("phone").unwrap();
        let mut methods = Vec::new();
        let mut replies = VecDeque::from([
            json!({"type":"status","status":observation("unlocked")}),
            json!({"type":"observed_navigation","observation":observation("unlocked")}),
        ]);
        let mut exchange = |bytes: &[u8]| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            methods.push(request["method"].as_str().unwrap().to_owned());
            Ok(serde_json::to_vec(&replies.pop_front().expect("extra capture")).unwrap())
        };
        tool_result(
            Tool::Status,
            Default::default(),
            &device,
            &mut session,
            &mut exchange,
        );
        let result = tool_result(
            Tool::Observe,
            presentation::Options {
                targets,
                tree_format: presentation::TreeFormat::Outline,
                image: presentation::ImageMode::Omit,
                ..Default::default()
            },
            &device,
            &mut session,
            &mut exchange,
        );
        assert_eq!(methods, ["status", "observe_navigation"]);
        assert_ne!(result["isError"], true);
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        let p = &result["structuredContent"];
        assert_eq!(p["revision_scope"], "navigation");
        assert_eq!(p["target_actionable"], false);
        assert_eq!(p["tree"]["included"], false);
        assert_eq!(p["image"]["reason"], "requested");
        assert!(session.target_inventory.is_none());
    }
}

#[test]
fn home_hint_cannot_replace_a_new_app_or_lock_screen_tree() {
    for (app, lock) in [
        ("example.app", "unlocked"),
        ("com.apple.springboard", "locked"),
    ] {
        let mut fresh = observation(lock);
        fresh["state"]["foreground_app"] = json!(app);
        let mut full = fresh.clone();
        full["accessibility_tree"] =
            json!("<Application><Button name=\"fresh content\"/></Application>");
        let mut replies = VecDeque::from([
            json!({"type":"observed_navigation","observation":fresh}),
            json!({"type":"observed","observation":full}),
        ]);
        let mut session = Session::default();
        session.home_observe_hint = true;
        let mut methods = vec![];
        let result = tool_result(
            Tool::Observe,
            presentation::Options {
                image: presentation::ImageMode::Omit,
                ..Default::default()
            },
            &DeviceKey::new("phone").unwrap(),
            &mut session,
            &mut |bytes| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                Ok(serde_json::to_vec(&replies.pop_front().expect("extra read")).unwrap())
            },
        );
        assert_eq!(methods, ["observe_navigation", "observe"]);
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(
            payload["accessibility_tree"]
                .as_str()
                .unwrap()
                .contains("fresh content")
        );
        assert_eq!(payload["metadata"]["state"]["foreground_app"], app);
        assert!(!session.home_observe_hint);
    }
}

#[test]
fn home_hint_keeps_fresh_capture_refusals_and_clears_target_authority() {
    let mut wrong_device = observation("unlocked");
    wrong_device["device"] = json!("other");
    for reply in [
        json!({"type":"error","code":"AdapterNativeBefore"}),
        json!({"type":"observed_navigation","observation":wrong_device}),
    ] {
        let mut session = Session::default();
        session.home_observe_hint = true;
        session.target_inventory = Some((
            Revision(3),
            serde_json::from_value(observation("unlocked")["state"].clone()).unwrap(),
            vec![],
        ));
        let mut calls = 0;
        let result = tool_result(
            Tool::Observe,
            Default::default(),
            &DeviceKey::new("phone").unwrap(),
            &mut session,
            &mut |bytes| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(request["method"], "observe_navigation");
                calls += 1;
                Ok(serde_json::to_vec(&reply).unwrap())
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(result["isError"], true);
        assert!(session.target_inventory.is_none());
        assert!(!session.home_observe_hint);
    }
}

#[test]
fn stale_home_hint_preserves_requested_native_targets() {
    let mut app = observation("unlocked");
    app["state"]["foreground_app"] = json!("example.app");
    let target = json!({"target_id":"t_0123456789abcdef0123456789abcdef",
        "source":"native","kind":"XCUIElementTypeButton","label":"Choose",
        "can_type":false,"text_only":false,"rect":[1.,2.,3.,4.],
        "value":null,"value_truncated":false});
    let mut replies = VecDeque::from([
        json!({"type":"observed_navigation","observation":app}),
        json!({"type":"observed_targets","observation":app,"targets":[target]}),
    ]);
    let mut session = Session::default();
    session.home_observe_hint = true;
    let mut methods = vec![];
    let result = tool_result(
        Tool::Observe,
        presentation::Options {
            targets: true,
            ..Default::default()
        },
        &DeviceKey::new("phone").unwrap(),
        &mut session,
        &mut |bytes| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            methods.push(request["method"].as_str().unwrap().to_owned());
            Ok(serde_json::to_vec(&replies.pop_front().unwrap()).unwrap())
        },
    );
    assert_eq!(methods, ["observe_navigation", "observe_targets"]);
    let payload: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["targets"], json!([target]));
    assert!(!session.home_observe_hint);
    assert!(session.target_inventory.is_some());
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
        "AdapterTargetBudget",
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

#[test]
fn cold_home_marker_requires_fresh_guarded_navigation_and_clears_authority() {
    for next in [
        json!({"type":"observed_navigation","observation":observation("unlocked")}),
        json!({"type":"error","code":"AdapterNativeBefore"}),
    ] {
        let mut session = Session::default();
        session.target_inventory = Some((
            Revision(3),
            serde_json::from_value(observation("unlocked")["state"].clone()).unwrap(),
            vec![],
        ));
        let fails = next["type"] == "error";
        let mut replies =
            VecDeque::from([json!({"type":"error","code":"AdapterCaptureHome"}), next]);
        let mut methods = vec![];
        let result = tool_result(
            Tool::Observe,
            presentation::Options {
                image: presentation::ImageMode::Omit,
                ..Default::default()
            },
            &DeviceKey::new("phone").unwrap(),
            &mut session,
            &mut |bytes| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                Ok(serde_json::to_vec(&replies.pop_front().expect("extra capture")).unwrap())
            },
        );
        assert_eq!(methods, ["observe", "observe_navigation"]);
        assert!(session.target_inventory.is_none());
        if fails {
            assert_eq!(result["isError"], true);
            assert!(!session.home_observe_hint);
        } else {
            assert_ne!(result["isError"], true);
            assert_eq!(result["structuredContent"]["revision_scope"], "navigation");
            assert_eq!(result["structuredContent"]["target_actionable"], false);
            assert!(session.home_observe_hint);
        }
    }
}
