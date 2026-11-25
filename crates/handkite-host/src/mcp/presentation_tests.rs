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
                json!({"method": if request["params"]["arguments"]["tree_format"] == "outline" {
                    "observe_outline"
                } else { "observe" }, "device":"phone"})
            );
            let mut reply: Value = serde_json::from_slice(captured).unwrap();
            if request["params"]["arguments"]["tree_format"] == "outline" {
                reply["type"] = json!("observed_outline");
                reply["navigation_only"] = json!(false);
            }
            Ok(serde_json::to_vec(&reply).unwrap())
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
        // Build the pre-presentation contract independently of production helpers.
        let original = json!({"jsonrpc":"2.0","id":2,"result":{
        "content":[
            {"type":"text","text":json!({"accessibility_tree":tree,"metadata":{
                "device":"phone","revision":3,
                "window":{"started_unix_ms":1,"finished_unix_ms":2},
                "state":{"lock":"locked","wda_ready":true,"foreground_app":null}
            }}).to_string()},
            {"type":"image","data":"AQID","mimeType":if format=="png" {"image/png"} else {"image/jpeg"}}
        ]}});
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
fn tree_omission_returns_the_same_image_from_one_paired_capture() {
    let capture = captured(
        "<AppiumAUT><XCUIElementTypeButton label='private'/></AppiumAUT>",
        "png",
    );
    let full = call(&request(json!({})), &capture);
    let omitted = call(&request(json!({"tree_format":"omit"})), &capture);
    let screen = payload(&omitted);
    assert_eq!(screen["metadata"], payload(&full)["metadata"]);
    assert_eq!(screen["capture_kind"], "paired");
    assert_eq!(
        screen["tree"],
        json!({"included":false,"reason":"requested"})
    );
    assert!(screen.get("accessibility_tree").is_none());
    assert_eq!(
        omitted["result"]["content"][1],
        full["result"]["content"][1]
    );
    assert!(!omitted.to_string().contains("private"));
}

#[test]
fn incomplete_action_omission_is_rejected_before_dispatch() {
    for response in [
        json!({"tree_format":"omit","image":"omit"}),
        json!({"tree_format":"omit","since_revision":1}),
    ] {
        let args = json!({"expected_revision":1,"at":{"x":1,"y":2},"response":response});
        assert!(!interaction::valid_raw(Tool::Tap, &args.to_string()));
        assert!(interaction::parse_response(Tool::Tap, args).is_err());
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
        json!({"tree_format":"omit","image":"omit"}),
        json!({"tree_format":"omit","image":"changed"}),
        json!({"tree_format":"omit","since_revision":3}),
        json!({"tree_format":"compact","extra":1}),
        json!({"since_revision":null}),
        json!({"since_revision":-1}),
        json!({"since_revision":1.5}),
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
        r#"{"since_revision":3,"since_revision":3}"#,
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
        json!(["full", "compact", "elements", "outline", "omit"])
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
    let (bytes, kept) = encode_response(response).unwrap();
    assert!(!kept);
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

#[test]
fn failed_or_oversized_responses_cannot_supply_a_delta_baseline() {
    let device = DeviceKey::new("phone").unwrap();
    for oversized in [false, true] {
        let tree = synthetic_tree(100, true, false);
        let mut first: Value = serde_json::from_slice(&captured(&tree, "png")).unwrap();
        if oversized {
            let plain = call(&request(json!({})), &serde_json::to_vec(&first).unwrap());
            let padding = MAX_RESPONSE - 2 - serde_json::to_vec(&plain).unwrap().len();
            first["observation"]["screenshot_base64"] =
                json!(format!("AQID{}", "A".repeat(padding)));
        }
        let mut last: Value = serde_json::from_slice(&captured(&tree, "png")).unwrap();
        last["observation"]["revision"] = json!(4);
        let mut replies = std::collections::VecDeque::from([serde_json::to_vec(&first).unwrap()]);
        let mut requests = vec![request(json!({"image":"changed"}))];
        if !oversized {
            replies.push_back(br#"{"type":"error","message":"Adapter"}"#.to_vec());
            requests.push(request(json!({})));
        }
        replies.push_back(serde_json::to_vec(&last).unwrap());
        requests.push(request(json!({"since_revision":3,"image":"changed"})));
        let input: String = requests
            .iter()
            .map(|request| format!("{request}\n"))
            .collect();
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, &device, |_| {
            Ok(replies.pop_front().unwrap())
        })
        .unwrap();
        assert!(replies.is_empty());
        let output: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let failed = &output[if oversized { 0 } else { 1 }];
        assert_eq!(failed["result"]["isError"], true);
        let last = output.last().unwrap();
        assert_eq!(payload(last)["tree_delta_reset"], "baseline_unavailable");
        assert_eq!(payload(last)["accessibility_tree"], tree);
        assert_eq!(last["result"]["content"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn selective_and_delta_synthetic_wire_bytes() {
    let tree = synthetic_tree(100, true, true);
    let mut first: Value = serde_json::from_slice(&captured(&tree, "png")).unwrap();
    first["observation"]["screenshot_base64"] = json!("A".repeat(65536));
    let first = serde_json::to_vec(&first).unwrap();
    let full = call(&request(json!({})), &first);
    let outline = call(
        &request(json!({"tree_format":"outline","image":"omit"})),
        &first,
    );
    let mut next: Value = serde_json::from_slice(&first).unwrap();
    next["type"] = json!("observed_outline");
    next["navigation_only"] = json!(false);
    let mut outline_first: Value = serde_json::from_slice(&first).unwrap();
    outline_first["type"] = json!("observed_outline");
    outline_first["navigation_only"] = json!(false);
    next["observation"]["revision"] = json!(4);
    next["observation"]["accessibility_tree"] = json!(tree.replacen("Café", "Coffee", 2));
    let mut session = Session::default();
    let device = DeviceKey::new("phone").unwrap();
    process(
        &request(json!({"tree_format":"outline","image":"changed"})),
        &device,
        Capabilities::Observation,
        &mut session,
        &mut |_| Ok(serde_json::to_vec(&outline_first).unwrap()),
    )
    .unwrap();
    let delta = process(
        &request(json!({"tree_format":"outline","image":"changed","since_revision":3})),
        &device,
        Capabilities::Observation,
        &mut session,
        &mut |_| Ok(serde_json::to_vec(&next).unwrap()),
    )
    .unwrap();
    let sizes = [&full, &outline, &delta].map(|value| serde_json::to_vec(value).unwrap().len());
    let text_sizes = [&full, &outline, &delta].map(|value| {
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .len()
    });
    assert!(sizes[2] < sizes[1] && sizes[1] < sizes[0]);
    assert!(text_sizes[2] < text_sizes[1] && text_sizes[1] < text_sizes[0]);
    assert!(payload(&delta).get("accessibility_tree_delta").is_some());
    eprintln!(
        "synthetic 100-control response bytes: full={} outline_no_image={} delta_unchanged_image={}; text bytes: full={} outline={} delta={}",
        sizes[0], sizes[1], sizes[2], text_sizes[0], text_sizes[1], text_sizes[2]
    );
}

#[test]
fn action_delta_uses_its_post_action_capture_and_preserves_the_receipt() {
    let mut before: Value =
        serde_json::from_slice(&captured(&synthetic_tree(100, true, false), "png")).unwrap();
    before["type"] = json!("observed_outline");
    before["navigation_only"] = json!(false);
    before["observation"]["state"] =
        json!({"lock":"unlocked","wda_ready":true,"foreground_app":"com.apple.mobiletimer"});
    let mut after = before["observation"].clone();
    after["revision"] = json!(5);
    after["accessibility_tree"] = json!(
        after["accessibility_tree"]
            .as_str()
            .unwrap()
            .replacen("Example", "Changed", 2)
    );
    let receipt = json!({"accepted":true,"action_kind":"tap"});
    let token = "0123456789abcdef0123456789abcdef";
    let mut replies = [before, json!({"type":"acquired","lease":token}),
        json!({"type":"acted","receipt":receipt,"after":after,"native_lock":null,"effect":"changed"})].into_iter();
    let mut calls = Vec::new();
    let mut exchange = |bytes: &[u8]| {
        calls.push(serde_json::from_slice::<Value>(bytes).unwrap());
        Ok(serde_json::to_vec(&replies.next().unwrap()).unwrap())
    };
    let mut session = Session::default();
    let device = DeviceKey::new("phone").unwrap();
    process(
        &request(json!({"tree_format":"outline","image":"changed"})),
        &device,
        Capabilities::ObservationNavigationInteraction,
        &mut session,
        &mut exchange,
    )
    .unwrap();
    let mut acquire = request(json!({}));
    acquire["params"]["name"] = json!("handkite_acquire_control");
    process(
        &acquire,
        &device,
        Capabilities::ObservationNavigationInteraction,
        &mut session,
        &mut exchange,
    )
    .unwrap();
    let mut action = request(json!({"expected_revision":3,"at":{"x":10,"y":20},
        "response":{"tree_format":"outline","image":"changed","since_revision":3}}));
    action["params"]["name"] = json!("handkite_tap");
    let result = process(
        &action,
        &device,
        Capabilities::ObservationNavigationInteraction,
        &mut session,
        &mut exchange,
    )
    .unwrap();
    assert_eq!(payload(&result)["receipt"], receipt);
    assert_eq!(result["result"]["content"].as_array().unwrap().len(), 2);
    let screen: Value =
        serde_json::from_str(result["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(screen["metadata"], payload(&result)["after"]);
    assert_eq!(screen["accessibility_tree_delta"]["base_revision"], 3);
    assert_eq!(screen["image"]["reason"], "unchanged");
    assert_eq!(
        calls
            .iter()
            .map(|call| call["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["observe_outline", "acquire", "act"]
    );
    assert_eq!(calls[2]["request"]["expected_revision"], 3);
    assert!(calls[2]["request"].get("response").is_none());
    assert!(!result.to_string().contains(token));
}

#[test]
fn visible_elements_are_structured_without_raw_xml_or_a_required_image() {
    let tree = "<AppiumAUT><XCUIElementTypeButton visible='true' enabled='true' label='Continue' x='1' y='2' width='30' height='40'/></AppiumAUT>";
    let reply = call(
        &request(json!({"tree_format":"elements","image":"omit"})),
        &captured(tree, "png"),
    );
    let value = payload(&reply);
    assert_eq!(reply["result"]["structuredContent"], value);
    assert_eq!(value["elements"][0]["role"], "Button");
    assert_eq!(value["elements"][0]["label"], "Continue");
    assert!(value.get("accessibility_tree").is_none());
    assert_eq!(reply["result"]["content"].as_array().unwrap().len(), 1);
}

#[test]
fn element_action_returns_effect_and_delta_in_one_text_payload() {
    let mut session = Session::default();
    let options: presentation::Options =
        serde_json::from_value(json!({"tree_format":"elements","image":"omit"})).unwrap();
    let tree = "<AppiumAUT><XCUIElementTypeTextField visible='true' enabled='true' label='Editor' value='A' x='1' y='2' width='30' height='40'/></AppiumAUT>";
    let mut capture: Value = serde_json::from_slice(&captured(tree, "png")).unwrap();
    session.presentation.content(
        serde_json::from_value(capture["observation"].clone()).unwrap(),
        options,
    );
    session.acquire(&mut |_| {
        Ok(serde_json::to_vec(
            &json!({"type":"acquired","lease":"0123456789abcdef0123456789abcdef"}),
        )
        .unwrap())
    });
    capture["observation"]["revision"] = json!(4);
    capture["observation"]["accessibility_tree"] = json!(tree.replace("value='A'", "value='AB'"));
    let mut count = 0;
    let reply = session.act(
        &DeviceKey::new("phone").unwrap(),
        Revision(3),
        Action::TextCurrent { value: "B".into() },
        true,
        serde_json::from_value(json!({"tree_format":"elements","image":"omit","since_revision":3}))
            .unwrap(),
        &mut |_| {
            count += 1;
            Ok(serde_json::to_vec(
                &json!({"type":"acted","receipt":{"accepted":true,"action_kind":"text"},
            "after":capture["observation"],"native_lock":null,"effect":"changed"}),
            )
            .unwrap())
        },
    );
    assert_eq!(count, 1);
    let value: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(reply["content"].as_array().unwrap().len(), 1);
    assert_eq!(value, reply["structuredContent"]);
    assert_eq!(value["effect"], "changed");
    assert_eq!(value["observation"]["metadata"]["revision"], 4);
    assert_eq!(
        value["observation"]["elements_delta"]["changed"][0]["value"],
        "AB"
    );
    assert!(!reply.to_string().contains("accessibility_tree"));
}
