use super::*;

fn observation(revision: u64, tree: &str) -> WireObservation {
    serde_json::from_value(json!({
        "device":"phone", "revision":revision,
        "window":{"started_unix_ms":1,"finished_unix_ms":2},
        "state":{"lock":"unlocked","wda_ready":true,"foreground_app":"example.app"},
        "accessibility_tree":tree,"screenshot_format":"png","screenshot_base64":"AQID"
    }))
    .unwrap()
}

fn options(value: Value) -> Options {
    serde_json::from_value(value).unwrap()
}

fn payload(content: &[Value]) -> Value {
    serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
}

fn tree(label: &str) -> String {
    (0..100)
        .map(|i| format!("item {i}: {}\n", if i == 50 { label } else { "unchanged" }))
        .collect()
}

fn apply(before: &str, delta: &Value) -> String {
    let mut lines: Vec<_> = before.split_inclusive('\n').collect();
    let start = delta["start_line"].as_u64().unwrap() as usize;
    let count = delta["delete_lines"].as_u64().unwrap() as usize;
    lines.splice(start..start + count, [delta["insert"].as_str().unwrap()]);
    lines.concat()
}

#[test]
fn line_splice_reconstructs_exact_bytes_including_unicode_and_newlines() {
    for (before, after) in [
        ("", ""),
        ("", "new\n"),
        ("old\n", ""),
        ("same\n", "same\n"),
        ("same\n", "same"),
        ("same", "same\n"),
        ("head\nold\ntail", "head\nCafé 🦊\ntail"),
        ("head\ntail\n", "head\nnew\ntail\n"),
        ("head\nremoved\ntail\n", "head\ntail\n"),
        ("a\r\nb\r\n", "a\r\n\u{754c}\r\n"),
    ] {
        let delta = line_delta(before, after, Revision(3)).unwrap();
        assert_eq!(delta["base_revision"], 3);
        assert_eq!(apply(before, &delta), after);
    }
}

#[test]
fn acknowledged_baseline_returns_smaller_lossless_tree_delta() {
    let mut cache = Cache::default();
    let first = tree("old");
    let second = tree("new 🦊");
    cache.content(observation(3, &first), Options::default());
    let content = cache.content(
        observation(4, &second),
        options(json!({"since_revision":3,"image":"changed"})),
    );
    let result = payload(&content);
    assert_eq!(content.len(), 1);
    assert_eq!(result["metadata"]["revision"], 4);
    assert!(result.get("accessibility_tree").is_none());
    assert_eq!(apply(&first, &result["accessibility_tree_delta"]), second);
    assert_eq!(result["image"]["reason"], "unchanged");

    let third = tree("third");
    let result = payload(&cache.content(
        observation(5, &third),
        options(json!({"since_revision":4,"image":"changed"})),
    ));
    assert_eq!(apply(&second, &result["accessibility_tree_delta"]), third);
}

#[test]
fn missing_or_mismatched_acknowledgement_never_sends_a_delta() {
    for acknowledgement in [None, Some(2), Some(4)] {
        let mut cache = Cache::default();
        cache.content(observation(3, &tree("old")), Options::default());
        let opts = acknowledgement.map_or_else(
            || options(json!({"image":"changed"})),
            |revision| options(json!({"since_revision":revision,"image":"changed"})),
        );
        let content = cache.content(observation(4, &tree("new")), opts);
        let result = payload(&content);
        assert_eq!(content.len(), 2);
        assert_eq!(result["accessibility_tree"], tree("new"));
        assert!(result.get("accessibility_tree_delta").is_none());
        if acknowledgement.is_some() {
            assert_eq!(result["tree_delta_reset"], "baseline_unavailable");
        }
    }
}

#[test]
fn identity_state_format_and_revision_changes_reset_the_baseline() {
    for change in 0..7 {
        let mut cache = Cache::default();
        cache.content(observation(3, &tree("old")), Options::default());
        let mut next = observation(4, &tree("new"));
        let mut opts = options(json!({"since_revision":3,"image":"changed"}));
        match change {
            0 => next.device = DeviceKey::new("other").unwrap(),
            1 => next.state = observation(0, "").state,
            2 => opts.tree_format = TreeFormat::Compact,
            3 => next.revision = Revision(3),
            4 => next.revision = Revision(2),
            5 => cache.clear(),
            6 => cache = Cache::default(),
            _ => unreachable!(),
        }
        if change == 1 {
            let mut wire = serde_json::to_value(next).unwrap();
            wire["state"]["foreground_app"] = json!("other.app");
            next = serde_json::from_value(wire).unwrap();
        }
        let content = cache.content(next, opts);
        assert_eq!(content.len(), 2, "case {change}");
        assert_eq!(
            payload(&content)["tree_delta_reset"],
            "baseline_unavailable"
        );
    }
}

#[test]
fn unseen_changed_and_differently_encoded_images_are_returned() {
    for change in 0..3 {
        let mut cache = Cache::default();
        let first_options = if change == 0 {
            options(json!({"image":"omit"}))
        } else {
            Options::default()
        };
        cache.content(observation(3, &tree("old")), first_options);
        let mut next = observation(4, &tree("new"));
        match change {
            1 => next.screenshot_base64 = "BAUG".into(),
            2 => next.screenshot_format = handkite_core::protocol::ImageFormat::Jpeg,
            _ => (),
        }
        let content = cache.content(next, options(json!({"since_revision":3,"image":"changed"})));
        assert_eq!(content.len(), 2);
        assert_eq!(payload(&content)["image"]["included"], true);
    }
}

#[test]
fn large_trees_and_line_counts_fall_back_without_retaining_a_large_baseline() {
    for large in [
        "x".repeat(MAX_BASELINE + 1),
        "a\n".repeat(MAX_DELTA_LINES + 1),
    ] {
        let mut cache = Cache::default();
        cache.content(observation(3, &tree("old")), Options::default());
        let content = cache.content(
            observation(4, &large),
            options(json!({"since_revision":3,"image":"changed"})),
        );
        assert_eq!(payload(&content)["accessibility_tree"], large);
        assert_eq!(payload(&content)["tree_delta_reset"], "limit_exceeded");
        if large.len() > MAX_BASELINE {
            assert!(cache.last.is_none());
        }
    }
}

#[test]
fn tiny_tree_uses_full_representation_when_delta_costs_more() {
    let mut cache = Cache::default();
    cache.content(observation(3, "old"), Options::default());
    let content = cache.content(observation(4, "new"), options(json!({"since_revision":3})));
    assert_eq!(payload(&content)["accessibility_tree"], "new");
    assert_eq!(payload(&content)["tree_delta_reset"], "not_smaller");
}

#[test]
fn invalid_projection_cannot_become_a_baseline() {
    let mut cache = Cache::default();
    let opts = options(json!({"tree_format":"outline","image":"changed"}));
    cache.content(observation(3, "<unknown/>"), opts);
    assert!(cache.last.is_none());
    let content = cache.content(
        observation(4, "<unknown/>"),
        options(json!({"tree_format":"outline","since_revision":3,"image":"changed"})),
    );
    assert_eq!(content.len(), 2);
    assert_eq!(payload(&content)["accessibility_tree"], "<unknown/>");
    assert_eq!(
        payload(&content)["tree_delta_reset"],
        "baseline_unavailable"
    );
}

#[test]
fn revision_acknowledgement_rejects_null_fractional_and_negative_inputs() {
    for value in [Value::Null, json!(-1), json!(1.5), json!("3"), json!(true)] {
        assert!(serde_json::from_value::<Options>(json!({"since_revision":value})).is_err());
    }
    assert!(serde_json::from_str::<Options>(r#"{"since_revision":3,"since_revision":3}"#).is_err());
}
