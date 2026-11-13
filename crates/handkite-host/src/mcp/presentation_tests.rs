use super::*;
use std::io::Cursor;

fn captured(tree: &str, format: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"type":"observed", "observation":{
        "device":"phone","revision":3,
        "window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"lock":"locked","wda_ready":true,"foreground_app":null},
        "accessibility_tree":tree,"screenshot_format":format,"screenshot_base64":"AQID"
    }}))
    .unwrap()
}

fn request(arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"handkite_observe","arguments":arguments}})
}

fn call(request: &Value, captured: &[u8]) -> Value {
    let mut output = Vec::new();
    let mut calls = 0;
    serve(
        Cursor::new(format!("{request}\n")),
        &mut output,
        &DeviceKey::new("phone").unwrap(),
        |bytes| {
            calls += 1;
            assert_eq!(
                serde_json::from_slice::<Value>(bytes).unwrap(),
                json!({"method":"observe","device":"phone"})
            );
            Ok(captured.to_vec())
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
    serde_json::from_slice(&output).unwrap()
}

fn payload(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn default_bytes_unchanged_and_compact_reuses_exact_capture() {
    let tree = "<AppiumAUT><XCUIElementTypeButton label='Café &amp; 🦊' enabled='true' x='1' y='2' width='3' height='4'/></AppiumAUT>";
    for format in ["png", "jpeg"] {
        let capture = captured(tree, format);
        let full = call(&request(json!({})), &capture);
        let mut omitted = request(json!({}));
        omitted["params"]
            .as_object_mut()
            .unwrap()
            .remove("arguments");
        assert_eq!(call(&omitted, &capture), full);
        let Reply::Observed { observation } = serde_json::from_slice(&capture).unwrap() else {
            panic!()
        };
        let original = json!({"jsonrpc":"2.0","id":2,"result":{
            "content":response::observation_content(observation)}});
        assert_eq!(
            serde_json::to_vec(&full).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
        let compact = call(&request(json!({"tree_format":"compact"})), &capture);
        let (full_payload, compact_payload) = (payload(&full), payload(&compact));
        assert_eq!(full_payload["accessibility_tree"], tree);
        assert_eq!(
            compact_payload["accessibility_tree"].as_str().unwrap(),
            compact::project(tree).unwrap()
        );
        assert_eq!(compact_payload["metadata"], full_payload["metadata"]);
        assert_eq!(compact_payload["tree_format"], "compact");
        assert_eq!(compact_payload["tree_format_version"], 1);
        assert_eq!(compact_payload["tree_format_reason"], "requested");
        assert_eq!(
            compact["result"]["content"][1],
            full["result"]["content"][1]
        );
        assert!(full_payload.get("tree_format").is_none());
    }
}

#[test]
fn fallback_keeps_the_same_full_tree_and_image() {
    for (tree, reason) in [
        ("<AppiumAUT>", "malformed_xml"),
        ("<AppiumAUT private='value'/>", "unsupported_xml"),
        (
            &format!("<AppiumAUT>{}</AppiumAUT>", "x".repeat(65537)),
            "limit_exceeded",
        ),
    ] {
        let capture = captured(tree, "png");
        let full = call(&request(json!({})), &capture);
        let fallback = call(&request(json!({"tree_format":"compact"})), &capture);
        let projected = payload(&fallback);
        assert_eq!(projected["accessibility_tree"], tree);
        assert_eq!(projected["tree_format"], "full");
        assert_eq!(projected["tree_format_version"], 1);
        assert_eq!(projected["tree_format_reason"], reason);
        assert_eq!(projected["metadata"], payload(&full)["metadata"]);
        assert_eq!(
            fallback["result"]["content"][1],
            full["result"]["content"][1]
        );
    }
}

#[test]
fn strict_arguments_refuse_before_exchange_in_both_validators() {
    let device = DeviceKey::new("phone").unwrap();
    for args in [
        json!(null),
        json!([]),
        json!({"tree_format":null}),
        json!({"tree_format":true}),
        json!({"tree_format":"full"}),
        json!({"tree_format":"compact","extra":1}),
    ] {
        let request = request(args);
        assert!(!valid_tool_frame(&serde_json::to_vec(&request).unwrap()));
        assert_eq!(
            process(
                &request,
                &device,
                Capabilities::Observation,
                &mut Session::default(),
                &mut |_| panic!("invalid arguments exchanged")
            )
            .unwrap()["error"]["code"],
            -32602
        );
    }
    for args in [
        r#"{"tree_format":"compact","tree_format":"compact"}"#,
        r#"{"tree_format":"other","tree_format":"compact"}"#,
    ] {
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"handkite_observe","arguments":{args}}}}}
"#
        );
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, &device, |_| {
            panic!("duplicate arguments exchanged")
        })
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output).unwrap()["error"]["code"],
            -32602
        );
    }
    let mut status = request(json!({"tree_format":"compact"}));
    status["params"]["name"] = json!("handkite_status");
    assert!(!valid_tool_frame(&serde_json::to_vec(&status).unwrap()));
    assert_eq!(
        process(
            &status,
            &device,
            Capabilities::Observation,
            &mut Session::default(),
            &mut |_| panic!("status arguments exchanged")
        )
        .unwrap()["error"]["code"],
        -32602
    );
}

#[test]
fn compact_does_not_bypass_capture_or_device_failure() {
    let device = DeviceKey::new("phone").unwrap();
    for capture in [br#"{"type":"error","message":"unavailable"}"#.to_vec(), {
        let mut capture: Value = serde_json::from_slice(&captured("<AppiumAUT/>", "png")).unwrap();
        capture["observation"]["device"] = json!("other");
        serde_json::to_vec(&capture).unwrap()
    }] {
        let reply = process(
            &request(json!({"tree_format":"compact"})),
            &device,
            Capabilities::Observation,
            &mut Session::default(),
            &mut |_| Ok(capture.clone()),
        )
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .find("AppiumAUT")
                .is_none()
        );
    }
}

#[test]
fn catalog_exposes_only_optional_compact_on_observe() {
    let catalog = catalog::catalog(Capabilities::Observation);
    let tools = catalog["tools"].as_array().unwrap();
    let schema = &tools
        .iter()
        .find(|tool| tool["name"] == "handkite_observe")
        .unwrap()["inputSchema"];
    assert_eq!(
        schema["properties"]["tree_format"]["enum"],
        json!(["compact"])
    );
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema.get("required").is_none());
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool["name"] == "handkite_status")
            .unwrap()["inputSchema"]["properties"],
        json!({})
    );
}

#[test]
fn representative_synthetic_wire_bytes() {
    let button = "<XCUIElementTypeButton type=\"XCUIElementTypeButton\" name=\"Example\" label=\"Example\" enabled=\"true\" visible=\"true\" x=\"10\" y=\"20\" width=\"100\" height=\"40\"/>";
    let capture = captured(
        &format!("<AppiumAUT>{}</AppiumAUT>", button.repeat(100)),
        "png",
    );
    let full = serde_json::to_vec(&call(&request(json!({})), &capture))
        .unwrap()
        .len();
    let compact = serde_json::to_vec(&call(&request(json!({"tree_format":"compact"})), &capture))
        .unwrap()
        .len();
    eprintln!("synthetic MCP bytes: full={full} compact={compact}");
    assert!(compact < full);
}
