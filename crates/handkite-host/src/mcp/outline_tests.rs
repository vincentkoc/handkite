use super::*;

fn observation(app: &str) -> Value {
    json!({"device":"phone","revision":4,"window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"lock":"unlocked","wda_ready":true,"foreground_app":app},
        "accessibility_tree":"<Application><Text label=\"unknown visibility\"/></Application>",
        "screenshot_format":"png","screenshot_base64":"AQID"})
}

#[test]
fn presentation_outline_keeps_content_and_discards_action_handles() {
    let mut session = Session::default();
    session.target_inventory = Some((
        Revision(3),
        serde_json::from_value(observation("old.app")["state"].clone()).unwrap(),
        vec![],
    ));
    let mut calls = Vec::new();
    let result = tool_result(
        Tool::Observe,
        serde_json::from_value(json!({
            "tree_format":"outline","image":"omit","targets":false,"since_revision":3
        }))
        .unwrap(),
        &DeviceKey::new("phone").unwrap(),
        &mut session,
        &mut |bytes| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            calls.push(request["method"].as_str().unwrap().to_owned());
            Ok(serde_json::to_vec(&json!({"type":"observed_outline",
            "observation":observation("com.apple.mobilesafari"),"navigation_only":true}))
            .unwrap())
        },
    );
    assert_eq!(calls, ["observe_outline"]);
    let body = &result["structuredContent"];
    assert!(
        body["accessibility_tree"]
            .as_str()
            .unwrap()
            .contains("unknown visibility")
    );
    assert_eq!(body["revision_scope"], "navigation");
    assert_eq!(body["metadata"]["revision_scope"], "navigation");
    assert_eq!(body["tree_scope"], "presentation");
    assert_eq!(body["visibility"], "unknown");
    assert_eq!(body["allowed_actions"], json!(["home", "launch_app"]));
    assert_eq!(body["target_actionable"], false);
    assert!(body.get("targets").is_none());
    assert!(session.target_inventory.is_none());
    assert!(
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["type"] != "image")
    );
}

#[test]
fn regular_outline_has_one_request_and_unchanged_scope() {
    let mut calls = 0;
    let result = tool_result(
        Tool::Observe,
        serde_json::from_value(json!({"tree_format":"outline","image":"omit"})).unwrap(),
        &DeviceKey::new("phone").unwrap(),
        &mut Session::default(),
        &mut |bytes| {
            calls += 1;
            assert_eq!(
                serde_json::from_slice::<Value>(bytes).unwrap()["method"],
                "observe_outline"
            );
            Ok(
                serde_json::to_vec(&json!({"type":"observed_outline", "navigation_only":false,
                "observation":observation("com.apple.mobiletimer")}))
                .unwrap(),
            )
        },
    );
    assert_eq!(calls, 1);
    let body: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(body.get("revision_scope").is_none());
    assert!(body.get("tree_scope").is_none());
}
