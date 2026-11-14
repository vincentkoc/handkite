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
