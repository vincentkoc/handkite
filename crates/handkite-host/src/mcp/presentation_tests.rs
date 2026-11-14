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
        (" \n<?xml version='1.0'?><AppiumAUT/>", "malformed_xml"),
        (
            "<?xml version='1.0'encoding='UTF-8'?><AppiumAUT/>",
            "malformed_xml",
        ),
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
fn image_omission_keeps_fresh_metadata_and_tree_without_another_exchange() {
    let tree = "<AppiumAUT><XCUIElementTypeButton label='Close'/></AppiumAUT>";
    let capture = captured(tree, "png");
    let reply = call(
        &request(json!({"tree_format":"compact","image":"omit"})),
        &capture,
    );
    assert_eq!(reply["result"]["content"].as_array().unwrap().len(), 1);
    let screen = payload(&reply);
    assert_eq!(screen["metadata"]["revision"], 3);
    assert_eq!(
        screen["accessibility_tree"],
        "<AppiumAUT><Button label='Close'/></AppiumAUT>"
    );
    assert_eq!(
        screen["image"],
        json!({"included":false,"reason":"requested"})
    );
    for options in [
        json!({"image":null}),
        json!({"image":"auto"}),
        json!({"image":false}),
    ] {
        assert!(!valid_tool_frame(
            &serde_json::to_vec(&request(options)).unwrap()
        ));
    }
}

#[test]
fn outline_is_explicitly_lossy_and_falls_back_to_the_original() {
    let tree = r#"<AppiumAUT><XCUIElementTypeButton name="Close" label="Close"/><XCUIElementTypeButton label="Hidden" visible="false"/></AppiumAUT>"#;
    let reply = call(
        &request(json!({"tree_format":"outline","image":"omit"})),
        &captured(tree, "png"),
    );
    let screen = payload(&reply);
    assert_eq!(screen["tree_format"], "outline");
    assert_eq!(screen["tree_lossy"], true);
    assert_eq!(screen["tree_omissions"]["invisible_nodes"], 1);
    assert!(
        screen["accessibility_tree"]
            .as_str()
            .unwrap()
            .contains("name=label=\"Close\"")
    );
    let invalid = "<unknown/>";
    let fallback = call(
        &request(json!({"tree_format":"outline"})),
        &captured(invalid, "png"),
    );
    assert_eq!(payload(&fallback)["tree_format"], "full");
    assert_eq!(payload(&fallback)["accessibility_tree"], invalid);
    assert_eq!(fallback["result"]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn strict_arguments_refuse_before_exchange_in_both_validators() {
    let device = DeviceKey::new("phone").unwrap();
    for args in [
        json!(null),
        json!([]),
        json!({"tree_format":null}),
        json!({"tree_format":true}),
        json!({"tree_format":"unknown"}),
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
        json!(["full", "compact", "outline"])
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

fn synthetic_tree(buttons: usize, pretty: bool, unicode: bool) -> String {
    let label = if unicode {
        "Café &amp; 🦊"
    } else {
        "Example"
    };
    let button = format!(
        "<XCUIElementTypeButton type=\"XCUIElementTypeButton\" name=\"{label}\" label=\"{label}\" enabled=\"true\" visible=\"true\" x=\"10\" y=\"20\" width=\"100\" height=\"40\"/>"
    );
    if pretty {
        format!(
            "<AppiumAUT>\n{}</AppiumAUT>\n",
            format!("  {button}\n").repeat(buttons)
        )
    } else {
        format!("<AppiumAUT>{}</AppiumAUT>", button.repeat(buttons))
    }
}

#[test]
fn representative_synthetic_wire_bytes() {
    for buttons in [1, 100] {
        for pretty in [false, true] {
            for unicode in [false, true] {
                let tree = synthetic_tree(buttons, pretty, unicode);
                let capture = captured(&tree, "png");
                let full = call(&request(json!({})), &capture);
                let compact = call(&request(json!({"tree_format":"compact"})), &capture);
                assert_eq!(payload(&full)["accessibility_tree"], tree);
                assert_eq!(
                    payload(&compact)["accessibility_tree"],
                    tree.replace("XCUIElementType", "")
                );
                assert_eq!(
                    compact["result"]["content"][1],
                    full["result"]["content"][1]
                );
                let full_bytes = serde_json::to_vec(&full).unwrap().len();
                let compact_bytes = serde_json::to_vec(&compact).unwrap().len();
                eprintln!(
                    "synthetic MCP bytes: buttons={buttons} pretty={pretty} unicode={unicode} full={full_bytes} compact={compact_bytes}"
                );
                if buttons == 100 {
                    assert!(compact_bytes < full_bytes);
                }
            }
        }
    }
}

#[test]
fn oversized_compact_fallback_returns_an_error_without_closing_the_session() {
    let mut capture: Value = serde_json::from_slice(&captured("<unknown/>", "png")).unwrap();
    let full_request = request(json!({}));
    let full = process(
        &full_request,
        &DeviceKey::new("phone").unwrap(),
        Capabilities::Observation,
        &mut Session::default(),
        &mut |_| Ok(serde_json::to_vec(&capture).unwrap()),
    )
    .unwrap();
    let padding = MAX_RESPONSE - 2 - serde_json::to_vec(&full).unwrap().len();
    capture["observation"]["screenshot_base64"] = json!(format!("AQID{}", "A".repeat(padding)));
    let capture = serde_json::to_vec(&capture).unwrap();
    assert!(capture.len() <= MAX_RESPONSE);
    let mut output = Vec::new();
    let input = format!(
        "{}\n{}\n",
        request(json!({"tree_format":"compact"})),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"})
    );
    let mut calls = 0;
    serve(
        Cursor::new(input),
        &mut output,
        &DeviceKey::new("phone").unwrap(),
        |_| {
            calls += 1;
            Ok(capture.clone())
        },
    )
    .unwrap();
    let replies: Vec<Value> = output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(calls, 1);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["result"]["isError"], true);
    assert_eq!(
        replies[0]["result"]["structuredContent"]["code"],
        "ResponseTooLarge"
    );
    assert!(replies[1]["result"]["tools"].is_array());
}

#[test]
fn oversized_action_keeps_the_receipt_without_replaying_or_inventing_delivery() {
    let receipt = json!({"receipt":{"accepted":true,"action_kind":"swipe"},
        "effect":"changed","after":{"revision":9}});
    let response = json!({"jsonrpc":"2.0","id":4,"result":{"content":[
        {"type":"text","text":receipt.to_string()},
        {"type":"text","text":"tree"},
        {"type":"image","data":"A".repeat(MAX_RESPONSE)}]}});
    let bytes = encode_response(response).unwrap();
    assert!(bytes.len() < MAX_RESPONSE);
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    let result = &reply["result"];
    assert_eq!(result["structuredContent"]["code"], "ResponseTooLarge");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        receipt
    );
}
