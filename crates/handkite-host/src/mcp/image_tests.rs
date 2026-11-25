use super::*;
use std::io::Cursor;

fn image() -> Value {
    json!({"device":"phone","window":{"started_unix_ms":1,"finished_unix_ms":2},
        "screenshot_format":"png","screenshot_base64":"AQID"})
}

fn request(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}})
}

fn text(content: &Value) -> Value {
    serde_json::from_str(content["text"].as_str().unwrap()).unwrap()
}

#[test]
fn explicit_image_capture_has_no_tree_state_or_revision() {
    let input = request(
        "handkite_observe",
        json!({"capture":"image_only","tree_format":"omit"}),
    );
    let mut output = Vec::new();
    serve(
        Cursor::new(format!("{input}\n")),
        &mut output,
        &DeviceKey::new("phone").unwrap(),
        |bytes| {
            assert_eq!(
                serde_json::from_slice::<Value>(bytes).unwrap(),
                json!({"method":"observe_image","device":"phone"})
            );
            Ok(
                serde_json::to_vec(&json!({"type":"observed_image","observation":image()}))
                    .unwrap(),
            )
        },
    )
    .unwrap();
    let reply: Value = serde_json::from_slice(&output).unwrap();
    let content = &reply["result"]["content"];
    let payload = text(&content[0]);
    assert_eq!(payload["capture_kind"], "image_only");
    assert_eq!(payload["tree"]["reason"], "not_captured");
    assert_eq!(
        payload["metadata"],
        json!({"device":"phone","window":{"started_unix_ms":1,"finished_unix_ms":2}})
    );
    assert!(payload.get("accessibility_tree").is_none());
    assert_eq!(content[1]["type"], "image");
}

#[test]
fn paired_image_drift_returns_navigation_revision_without_stale_tree() {
    for format in ["full", "elements"] {
        let mut session = Session::default();
        let observation = || {
            serde_json::from_value::<crate::wire::WireObservation>(json!({
                "device":"phone","revision":3,
                "window":{"started_unix_ms":1,"finished_unix_ms":2},
                "state":{"lock":"unlocked","wda_ready":true,"foreground_app":"fixture.app"},
                "accessibility_tree":"<Application/>",
                "screenshot_format":"png","screenshot_base64":"AQID"
            }))
            .unwrap()
        };
        session
            .presentation
            .content(observation(), presentation::Options::default());
        let options = serde_json::from_value(json!({
            "tree_format":format,"image":"omit","since_revision":3
        }))
        .unwrap();
        let mut calls = Vec::new();
        let result = tool_result(
            Tool::Observe,
            options,
            &DeviceKey::new("phone").unwrap(),
            &mut session,
            &mut |bytes| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                calls.push(request["method"].as_str().unwrap().to_owned());
                let reply = match request["method"].as_str().unwrap() {
                    "observe" | "observe_outline" | "observe_targets" => {
                        json!({"type":"error","code":"AdapterCaptureImageDrift"})
                    }
                    "observe_navigation" => json!({"type":"observed_navigation","observation":{
                        "device":"phone","revision":4,"window":{"started_unix_ms":1,"finished_unix_ms":2},
                        "state":{"lock":"unlocked","wda_ready":true,"foreground_app":"fixture.app"},
                        "accessibility_tree":"","screenshot_format":"png","screenshot_base64":"AQID"}}),
                    _ => panic!("no retries or input"),
                };
                Ok(serde_json::to_vec(&reply).unwrap())
            },
        );
        assert_eq!(
            calls,
            if format == "elements" {
                vec!["observe", "observe_targets", "observe_navigation"]
            } else if format == "outline" {
                vec!["observe_outline", "observe_navigation"]
            } else {
                vec!["observe", "observe_navigation"]
            }
        );
        assert_ne!(result["isError"], true);
        let payload = text(&result["content"][0]);
        assert_eq!(payload["capture_kind"], "image_only");
        assert_eq!(payload["tree"]["reason"], "capture_image_drift");
        assert_eq!(payload["target_actionable"], false);
        assert_eq!(payload["revision_scope"], "navigation");
        assert_eq!(payload["metadata"]["revision"], 4);
        assert_eq!(payload["allowed_actions"], json!(["home", "launch_app"]));
        for key in ["accessibility_tree", "elements", "targets"] {
            assert!(payload.get(key).is_none());
            assert!(payload["metadata"].get(key).is_none());
        }
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        assert_eq!(payload["image"]["reason"], "requested");
        if format == "elements" {
            assert_eq!(result["structuredContent"], payload);
        }
        let options = serde_json::from_value(json!({"since_revision":3})).unwrap();
        let next = session.presentation.content(observation(), options);
        assert_eq!(text(&next[0])["tree_delta_reset"], "baseline_unavailable");
    }
}

#[test]
fn only_ordinary_read_only_image_drift_selects_fallback() {
    for (arguments, code, method) in [
        (json!({}), "Adapter", "observe"),
        (json!({}), "AdapterIdentityCheck", "observe"),
        (json!({}), "DeviceMismatch", "observe"),
        (json!({}), "AdapterPreActionCapture", "observe"),
        (
            json!({"capture":"image_only","tree_format":"omit"}),
            "AdapterCaptureImageDrift",
            "observe_image",
        ),
    ] {
        let mut calls = 0;
        let result = tool_result(
            Tool::Observe,
            serde_json::from_value(arguments).unwrap(),
            &DeviceKey::new("phone").unwrap(),
            &mut Session::default(),
            &mut |bytes| {
                calls += 1;
                let request: Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(request["method"], method);
                Ok(serde_json::to_vec(&json!({"type":"error","code":code})).unwrap())
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["code"], code);
    }
}

#[test]
fn drift_fallback_refuses_wrong_device_or_failed_image_without_retry() {
    let mut wrong_device = image();
    wrong_device["device"] = json!("other-phone");
    for fallback in [
        json!({"type":"error","code":"AdapterCaptureImageDrift"}),
        json!({"type":"error","code":"DeviceMismatch"}),
        json!({"type":"observed_navigation","observation": {"device":"other-phone","revision":4,"window":{"started_unix_ms":1,"finished_unix_ms":2},"state":{"lock":"unlocked","wda_ready":true,"foreground_app":"fixture.app"},"accessibility_tree":"","screenshot_format":"png","screenshot_base64":"AQID"}}),
        json!({"type":"invalid"}),
    ] {
        let mut calls = 0;
        let result = tool_result(
            Tool::Observe,
            presentation::Options::default(),
            &DeviceKey::new("phone").unwrap(),
            &mut Session::default(),
            &mut |bytes| {
                calls += 1;
                let request: Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(
                    request["method"],
                    if calls == 1 {
                        "observe"
                    } else {
                        "observe_navigation"
                    }
                );
                assert!(calls <= 2);
                let reply = if calls == 1 {
                    json!({"type":"error","code":"AdapterCaptureImageDrift"})
                } else {
                    fallback.clone()
                };
                Ok(serde_json::to_vec(&reply).unwrap())
            },
        );
        assert_eq!(calls, 2);
        assert_eq!(result["isError"], true);
        assert!(
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "image")
        );
    }
}

#[test]
fn incompatible_image_options_refuse_before_exchange() {
    for arguments in [
        json!({"capture":"image_only"}),
        json!({"capture":"image_only","tree_format":"omit","image":"omit"}),
        json!({"capture":"image_only","tree_format":"omit","since_revision":3}),
        json!({"capture":"unknown","tree_format":"omit"}),
    ] {
        let mut output = Vec::new();
        let input = request("handkite_observe", arguments);
        serve(
            Cursor::new(format!("{input}\n")),
            &mut output,
            &DeviceKey::new("phone").unwrap(),
            |_| panic!("must not exchange"),
        )
        .unwrap();
        let reply: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(reply["error"]["code"], -32602);
    }
}

#[test]
fn image_post_reply_keeps_receipt_and_respects_observation_capability() {
    for include in [true, false] {
        for changed in [true, false] {
            let lease = handkite_core::controller::LeaseId::try_from(
                "00000000000000000000000000000001".to_owned(),
            )
            .unwrap();
            let mut session = Session::default();
            let mut exchange = |bytes: &[u8]| {
                let request: Value = serde_json::from_slice(bytes).unwrap();
                let reply = match request["method"].as_str().unwrap() {
                    "acquire" => json!({"type":"acquired","lease":lease}),
                    "act" => {
                        assert_eq!(request["capture"], "image_only");
                        json!({"type":"acted","receipt":{"accepted":true,"action_kind":"tap"},
                            "after":null,"after_image":image(),"native_lock":null,
                            "effect":if changed {"changed"} else {"unconfirmed"}})
                    }
                    _ => panic!("unexpected call"),
                };
                Ok(serde_json::to_vec(&reply).unwrap())
            };
            session.acquire(&mut exchange);
            let options =
                serde_json::from_value(json!({"capture":"image_only","tree_format":"omit"}))
                    .unwrap();
            let result = session.act(
                &DeviceKey::new("phone").unwrap(),
                Revision(1),
                Action::Tap {
                    at: handkite_core::protocol::Point { x: 1., y: 1. },
                },
                include,
                options,
                &mut exchange,
            );
            let receipt = text(&result["content"][0]);
            assert_eq!(receipt["receipt"]["accepted"], true);
            assert!(receipt["after"].is_null());
            assert!(receipt["after_image"].get("revision").is_none());
            assert_eq!(result["isError"] == true, !changed);
            assert_eq!(
                result["content"].as_array().unwrap().len(),
                if include { 3 } else { 1 }
            );
        }
    }
}

#[test]
fn older_action_reply_still_decodes_without_image_field() {
    assert!(
        serde_json::from_value::<Reply>(json!({"type":"acted",
        "receipt":{"accepted":true,"action_kind":"tap"},"after":null,
        "native_lock":null,"effect":"unconfirmed"}))
        .is_ok()
    );
}
