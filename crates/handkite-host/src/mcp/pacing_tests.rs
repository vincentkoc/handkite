use super::*;
use crate::pacing::{Pacer, Policy};
use handkite_core::controller::LeaseId;
use handkite_core::protocol::Point;
use std::io::Cursor;

fn lease() -> LeaseId {
    LeaseId::try_from("1".repeat(32)).unwrap()
}

/// Run one session and return every daemon request it sent.
fn requests(pacer: Pacer, calls: &[Value]) -> (Vec<Value>, Vec<Value>) {
    requests_replying(pacer, calls, "StaleRevision")
}

/// As `requests`, with every action answered by error `code`.
fn requests_replying(pacer: Pacer, calls: &[Value], code: &str) -> (Vec<Value>, Vec<Value>) {
    let mut input = String::new();
    input.push_str(
        &json!({"jsonrpc":"2.0","id":0,"method":"tools/call",
            "params":{"name":"handkite_acquire_control","arguments":{}}})
        .to_string(),
    );
    input.push('\n');
    for (index, call) in calls.iter().enumerate() {
        let mut call = call.clone();
        call["id"] = json!(index + 1);
        input.push_str(&call.to_string());
        input.push('\n');
    }
    let mut output = Vec::new();
    let mut sent = Vec::new();
    serve_with_pacing(
        Cursor::new(input.into_bytes()),
        &mut output,
        &DeviceKey::new("phone").unwrap(),
        Capabilities::ObservationNavigationInteraction,
        pacer,
        |request| {
            let request: Value = serde_json::from_slice(request).unwrap();
            let reply = match request["method"].as_str() {
                Some("acquire") => Reply::Acquired { lease: lease() },
                Some("release") => Reply::Released,
                _ => Reply::Error { code: code.into() },
            };
            sent.push(request);
            Ok(serde_json::to_vec(&reply).unwrap())
        },
    )
    .unwrap();
    let replies = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let actions = sent
        .into_iter()
        .filter(|request| matches!(request["method"].as_str(), Some("act" | "navigate")))
        .collect();
    (actions, replies)
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":0,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}})
}

#[test]
fn off_sends_the_unchanged_request() {
    let tap = call(
        "handkite_tap",
        json!({"expected_revision":7,"at":{"x":12.5,"y":90}}),
    );
    for pacer in [
        Pacer::default(),
        Pacer::new(Policy::Natural, 1).without_sleep(),
    ] {
        let off = pacer.policy() != Policy::Off;
        let calls = if off {
            // A paced session can still opt out per call.
            vec![call(
                "handkite_tap",
                json!({"expected_revision":7,"at":{"x":12.5,"y":90},"pacing":{"policy":"off"}}),
            )]
        } else {
            vec![tap.clone()]
        };
        let (sent, _) = requests(pacer, &calls);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].get("pacing").is_none());
        assert_eq!(
            sent[0]["request"]["action"],
            json!({"type":"tap","at":{"x":12.5,"y":90.0}})
        );
        assert_eq!(sent[0]["request"]["expected_revision"], 7);
    }
}

#[test]
fn per_call_override_shapes_and_replays_from_its_seed() {
    let calls = [
        call(
            "handkite_swipe",
            json!({"expected_revision":3,"from":{"x":200,"y":700},"to":{"x":200,"y":200},
                "duration_ms":400,"pacing":{"policy":"natural","seed":11}}),
        ),
        call(
            "handkite_text",
            json!({"expected_revision":4,"value":"hello","pacing":{"policy":"cautious","seed":12}}),
        ),
        call(
            "handkite_home",
            json!({"expected_revision":5,"pacing":{"policy":"natural","seed":13}}),
        ),
    ];
    let (first, replies) = requests(Pacer::default().without_sleep(), &calls);
    let (second, _) = requests(Pacer::default().without_sleep(), &calls);
    assert_eq!(first, second);
    assert_eq!(first.len(), 3);
    assert_eq!(first[0]["pacing"]["policy"], "natural");
    assert!(first[0]["pacing"]["seed"].is_u64());
    let duration = first[0]["request"]["action"]["duration_ms"]
        .as_u64()
        .unwrap();
    assert!((320..=490).contains(&duration), "{duration}");
    // Endpoints are the caller's; the daemon re-admits the action as sent.
    assert_eq!(
        first[0]["request"]["action"]["from"],
        json!({"x":200.0,"y":700.0})
    );
    assert_eq!(first[1]["pacing"]["policy"], "cautious");
    assert_eq!(first[1]["request"]["action"]["value"], "hello");
    // Navigation is only delayed; its request carries no shape.
    assert_eq!(first[2]["method"], "navigate");
    assert!(first[2].get("pacing").is_none());
    // Revision checks are untouched: the daemon refusal reaches the caller.
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
}

#[test]
fn tap_jitter_stays_inside_the_observed_frame() {
    let lease = lease();
    let device = DeviceKey::new("phone").unwrap();
    for seed in 0..200 {
        let mut session = Session::with_pacing(Pacer::new(Policy::Natural, seed).without_sleep());
        session.lease = Some(lease);
        session.target_inventory = Some((
            Revision(9),
            handkite_core::protocol::DeviceState {
                lock: handkite_core::protocol::LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some("com.example.app".into()),
            },
            vec![handkite_core::controller::ElementTarget {
                target_id: format!("t_{}", "0".repeat(32)),
                source: "xml".into(),
                kind: "Button".into(),
                label: "Fixture".into(),
                can_type: false,
                text_only: false,
                rect: [100.0, 200.0, 44.0, 30.0],
                value: None,
                value_truncated: false,
            }],
        ));
        let at = Point { x: 122.0, y: 215.0 };
        let action = session.pace(None, Revision(9), Action::Tap { at });
        let Action::Tap { at: moved } = action.clone() else {
            panic!()
        };
        assert!(moved.x > 100.0 && moved.x < 144.0, "{moved:?}");
        assert!(moved.y > 200.0 && moved.y < 230.0, "{moved:?}");
        let mut sent = None;
        session.act(
            &device,
            Revision(9),
            action,
            false,
            presentation::Options::default(),
            &mut |request| {
                sent = Some(serde_json::from_slice::<Value>(request).unwrap());
                Ok(serde_json::to_vec(&Reply::Error {
                    code: "StaleRevision".into(),
                })
                .unwrap())
            },
        );
        let sent = sent.unwrap();
        assert_eq!(sent["pacing"]["policy"], "natural");
        assert_eq!(sent["request"]["expected_revision"], 9);
        // Frames from another revision are never used.
        let stale = session.pace(None, Revision(8), Action::Tap { at });
        assert!(matches!(stale, Action::Tap { at: same } if same == at));
    }
}

#[test]
fn schema_and_raw_frame_accept_only_a_well_formed_override() {
    let catalog = catalog::catalog(Capabilities::ObservationNavigationInteraction);
    for tool in catalog["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let has = tool["inputSchema"]["properties"].get("pacing").is_some();
        let action = Tool::from_name(name).unwrap().is_action();
        assert_eq!(has, action, "{name}");
        if has {
            assert_eq!(
                tool["inputSchema"]["properties"]["pacing"]["properties"]["policy"]["enum"],
                json!(["off", "natural", "cautious"])
            );
        }
    }
    let frame = |pacing: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"handkite_tap","arguments":{{"expected_revision":1,"at":{{"x":1,"y":2}},"pacing":{pacing}}}}}}}"#
        )
    };
    assert!(valid_tool_frame(
        frame(r#"{"policy":"natural","seed":1}"#).as_bytes()
    ));
    assert!(valid_tool_frame(frame("{}").as_bytes()));
    for bad in [
        r#"{"policy":"fast"}"#,
        r#"{"policy":"natural","policy":"off"}"#,
        r#"{"seed":-1}"#,
        r#""natural""#,
        r#"{"extra":true}"#,
    ] {
        assert!(!valid_tool_frame(frame(bad).as_bytes()), "{bad}");
    }
    // Native target tools take no override.
    let element = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"handkite_tap_element","arguments":{{"expected_revision":1,"target_id":"t_{}","pacing":{{}}}}}}}}"#,
        "0".repeat(32)
    );
    assert!(!valid_tool_frame(element.as_bytes()));
}

fn payload(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn daemon_without_pacing_is_a_typed_pre_input_refusal() {
    // An older daemon rejects the unknown `pacing` field at its socket, before
    // any dispatch, with `InvalidRequest`.
    for (name, arguments) in [
        (
            "handkite_tap",
            json!({"expected_revision":7,"at":{"x":12.5,"y":90},"pacing":{"policy":"natural","seed":1}}),
        ),
        (
            "handkite_swipe",
            json!({"expected_revision":7,"from":{"x":1,"y":2},"to":{"x":3,"y":400},"duration_ms":300,
                "pacing":{"policy":"cautious"}}),
        ),
        (
            "handkite_text",
            json!({"expected_revision":7,"value":"hi","pacing":{"policy":"natural"}}),
        ),
    ] {
        let (sent, replies) = requests_replying(
            Pacer::default().without_sleep(),
            &[call(name, arguments)],
            "InvalidRequest",
        );
        // Exactly one request: never resent with or without pacing.
        assert_eq!(sent.len(), 1, "{name}");
        assert!(sent[0].get("pacing").is_some());
        let reply = &replies[1];
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(
            reply["result"]["structuredContent"]["code"],
            "PacingUnsupported"
        );
        let payload = payload(reply);
        assert_eq!(payload["delivery"], "not_sent", "{name}");
        assert_eq!(payload["receipt"]["accepted"], false);
        assert_eq!(payload["effect"], "unconfirmed");
        assert!(reply.to_string().contains("Retry with pacing off"));
    }
}

#[test]
fn unpaced_invalid_request_handling_is_unchanged() {
    for pacer in [
        Pacer::default(),
        Pacer::new(Policy::Natural, 1).without_sleep(),
    ] {
        // Unpaced, or a paced session whose action the daemon never shapes.
        let arguments = if pacer.policy() == Policy::Off {
            json!({"expected_revision":7,"at":{"x":12.5,"y":90}})
        } else {
            json!({"expected_revision":7,"key":"enter"})
        };
        let name = if pacer.policy() == Policy::Off {
            "handkite_tap"
        } else {
            "handkite_key"
        };
        let (sent, replies) = requests_replying(pacer, &[call(name, arguments)], "InvalidRequest");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].get("pacing").is_none());
        let payload = payload(&replies[1]);
        assert_eq!(payload["delivery"], "unknown");
        assert_eq!(payload["receipt"]["accepted"], Value::Null);
        assert!(replies[1]["result"].get("structuredContent").is_none());
    }
}
