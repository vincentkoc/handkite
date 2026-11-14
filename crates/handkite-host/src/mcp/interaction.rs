use super::catalog::Tool;
use handkite_core::protocol::{Action, ButtonTarget, KeyboardKey, Point, Revision};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionOnly {
    expected_revision: Revision,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenPoint {
    x: f64,
    y: f64,
}

impl ScreenPoint {
    fn valid(&self) -> bool {
        [self.x, self.y]
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=10_000.0).contains(&value))
    }

    fn into_point(self) -> Point {
        Point {
            x: self.x,
            y: self.y,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TapArguments {
    expected_revision: Revision,
    at: ScreenPoint,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ButtonArguments {
    expected_revision: Revision,
    target: ButtonTarget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LongPressArguments {
    expected_revision: Revision,
    at: ScreenPoint,
    duration_ms: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyArguments {
    expected_revision: Revision,
    key: KeyboardKey,
    selection: Option<KeySelection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeySelection {
    Current,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwipeArguments {
    expected_revision: Revision,
    from: ScreenPoint,
    to: ScreenPoint,
    duration_ms: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArguments {
    expected_revision: Revision,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchArguments {
    expected_revision: Revision,
    bundle_id: String,
}

pub(super) fn parse(tool: Tool, value: Value) -> Result<(Revision, Action), ()> {
    match tool {
        Tool::ClickButton => {
            let args: ButtonArguments = serde_json::from_value(value).map_err(|_| ())?;
            if !args.target.valid() {
                return Err(());
            }
            Ok((
                args.expected_revision,
                Action::ClickButton {
                    target: args.target,
                },
            ))
        }
        Tool::Home | Tool::Clock => {
            let args: RevisionOnly = serde_json::from_value(value).map_err(|_| ())?;
            let action = if tool == Tool::Home {
                Action::Home
            } else {
                Action::LaunchApp {
                    bundle_id: "com.apple.mobiletimer".into(),
                }
            };
            Ok((args.expected_revision, action))
        }
        Tool::Tap | Tool::DoubleTap => {
            let args: TapArguments = serde_json::from_value(value).map_err(|_| ())?;
            if !args.at.valid() {
                return Err(());
            }
            Ok((
                args.expected_revision,
                if tool == Tool::DoubleTap {
                    Action::DoubleTap {
                        at: args.at.into_point(),
                    }
                } else {
                    Action::Tap {
                        at: args.at.into_point(),
                    }
                },
            ))
        }
        Tool::LongPress => {
            let args: LongPressArguments = serde_json::from_value(value).map_err(|_| ())?;
            if !args.at.valid() || !(1..=5_000).contains(&args.duration_ms) {
                return Err(());
            }
            Ok((
                args.expected_revision,
                Action::LongPress {
                    at: args.at.into_point(),
                    duration_ms: args.duration_ms,
                },
            ))
        }
        Tool::Key => {
            let args: KeyArguments = serde_json::from_value(value).map_err(|_| ())?;
            let action = match args.selection {
                None => Action::Key { key: args.key },
                Some(KeySelection::Current) => Action::KeyCurrent { key: args.key },
            };
            Ok((args.expected_revision, action))
        }
        Tool::Swipe => {
            let args: SwipeArguments = serde_json::from_value(value).map_err(|_| ())?;
            if !args.from.valid() || !args.to.valid() || !(1..=5_000).contains(&args.duration_ms) {
                return Err(());
            }
            Ok((
                args.expected_revision,
                Action::Swipe {
                    from: args.from.into_point(),
                    to: args.to.into_point(),
                    duration_ms: args.duration_ms,
                },
            ))
        }
        Tool::Text => {
            let args: TextArguments = serde_json::from_value(value).map_err(|_| ())?;
            if args.value.is_empty()
                || args.value.len() > 4_096
                || args.value.chars().any(char::is_control)
            {
                return Err(());
            }
            Ok((args.expected_revision, Action::Text { value: args.value }))
        }
        Tool::LaunchApp => {
            let args: LaunchArguments = serde_json::from_value(value).map_err(|_| ())?;
            let id = args.bundle_id.as_bytes();
            if id.len() > 255
                || !id.contains(&b'.')
                || id.split(|byte| *byte == b'.').any(|part| part.is_empty())
                || !id[0].is_ascii_alphabetic()
                || !id
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
            {
                return Err(());
            }
            Ok((
                args.expected_revision,
                Action::LaunchApp {
                    bundle_id: args.bundle_id,
                },
            ))
        }
        _ => Err(()),
    }
}

pub(super) fn valid_raw(tool: Tool, raw: &str) -> bool {
    match tool {
        Tool::ClickButton => serde_json::from_str::<ButtonArguments>(raw).is_ok(),
        Tool::Home | Tool::Clock => serde_json::from_str::<RevisionOnly>(raw).is_ok(),
        Tool::Tap | Tool::DoubleTap => serde_json::from_str::<TapArguments>(raw).is_ok(),
        Tool::LongPress => serde_json::from_str::<LongPressArguments>(raw).is_ok(),
        Tool::Key => serde_json::from_str::<KeyArguments>(raw).is_ok(),
        Tool::Swipe => serde_json::from_str::<SwipeArguments>(raw).is_ok(),
        Tool::Text => serde_json::from_str::<TextArguments>(raw).is_ok(),
        Tool::LaunchApp => serde_json::from_str::<LaunchArguments>(raw).is_ok(),
        _ => false,
    }
}

pub(super) fn schema(tool: Tool) -> Value {
    let revision = json!({"type":"integer","minimum":0,"maximum":u64::MAX});
    let point = json!({"type":"object",
        "description":"WDA viewport coordinates in logical points, passed unchanged; not screenshot pixels. No screenshot-to-action transform is supplied.",
        "properties":{
        "x":{"type":"number","minimum":0,"maximum":10_000},
        "y":{"type":"number","minimum":0,"maximum":10_000}},
        "required":["x","y"],"additionalProperties":false});
    let mut properties = serde_json::Map::new();
    properties.insert("expected_revision".into(), revision);
    let mut required = vec!["expected_revision"];
    match tool {
        Tool::ClickButton => {
            properties.insert(
                "target".into(),
                json!({
                    "type":"object", "additionalProperties":false,
                    "properties":{
                        "label":{"type":"string","minLength":1,"maxLength":512},
                        "x":{"type":"number","minimum":0},
                        "y":{"type":"number","minimum":0},
                        "width":{"type":"number","exclusiveMinimum":0},
                        "height":{"type":"number","exclusiveMinimum":0}
                    },
                    "required":["label","x","y","width","height"]
                }),
            );
            required.push("target");
        }
        Tool::Tap | Tool::DoubleTap | Tool::LongPress => {
            properties.insert("at".into(), point);
            required.push("at");
            if tool == Tool::LongPress {
                properties.insert(
                    "duration_ms".into(),
                    json!({"type":"integer","minimum":1,"maximum":5_000}),
                );
                required.push("duration_ms");
            }
        }
        Tool::Swipe => {
            properties.insert("from".into(), point.clone());
            properties.insert("to".into(), point);
            properties.insert(
                "duration_ms".into(),
                json!({"type":"integer","minimum":1,"maximum":5_000}),
            );
            required.extend(["from", "to", "duration_ms"]);
        }
        Tool::Text => {
            properties.insert(
                "value".into(),
                json!({"type":"string","minLength":1,"description":"At most 4096 UTF-8 bytes; no control characters"}),
            );
            required.push("value");
        }
        Tool::Key => {
            properties.insert(
                "key".into(),
                json!({"type":"string","enum":["backspace","enter","tab","escape"]}),
            );
            required.push("key");
            properties.insert("selection".into(), json!({
                "type":"string", "enum":["current"],
                "description":"Opt in to applying the key to iOS's current editing selection. Does not preserve or prove the earlier caret/range. Omit for exact-image admission."
            }));
        }
        Tool::LaunchApp => {
            properties.insert(
                "bundle_id".into(),
                json!({"type":"string","minLength":3,"maxLength":255}),
            );
            required.push("bundle_id");
        }
        _ => {}
    }
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_selection_key_requires_explicit_opt_in() {
        let request = json!({"expected_revision":7,"key":"backspace"});
        let (_, strict) = parse(Tool::Key, request.clone()).unwrap();
        assert!(matches!(strict, Action::Key { .. }));
        let mut current = request.clone();
        current["selection"] = json!("current");
        let (_, current) = parse(Tool::Key, current).unwrap();
        assert!(matches!(
            current,
            Action::KeyCurrent {
                key: KeyboardKey::Backspace
            }
        ));
        let mut bad = request;
        bad["selection"] = json!("preserve");
        assert!(parse(Tool::Key, bad).is_err());
    }

    #[test]
    fn semantic_button_requires_exact_typed_arguments() {
        let valid = json!({"expected_revision":7,"target":{"label":"Close","x":338,"y":489,"width":38,"height":38}});
        let (_, action) = parse(Tool::ClickButton, valid.clone()).unwrap();
        assert_eq!(action.kind(), "click_button");
        for (key, value) in [
            ("label", json!("")),
            ("width", json!(0)),
            ("x", json!(-1)),
            ("id", json!("caller-id")),
        ] {
            let mut bad = valid.clone();
            bad["target"][key] = value;
            assert!(parse(Tool::ClickButton, bad).is_err(), "{key}");
        }
        let duplicate = r#"{"expected_revision":7,"target":{"label":"Close","label":"Other","x":338,"y":489,"width":38,"height":38}}"#;
        assert!(!valid_raw(Tool::ClickButton, duplicate));
    }

    #[test]
    fn tap_and_swipe_share_fractional_viewport_point_schema() {
        let tap = schema(Tool::Tap);
        let swipe = schema(Tool::Swipe);
        let point = &tap["properties"]["at"];
        assert_eq!(point, &swipe["properties"]["from"]);
        assert_eq!(point, &swipe["properties"]["to"]);
        assert_eq!(point["properties"]["x"]["type"], "number");
        assert_eq!(point["properties"]["y"]["type"], "number");
        let description = point["description"].as_str().unwrap();
        assert!(description.contains("logical points, passed unchanged"));
        assert!(description.contains("not screenshot pixels"));
        assert!(description.contains("No screenshot-to-action transform"));
    }

    #[test]
    fn each_tool_translates_to_one_typed_action() {
        for (tool, arguments, expected) in [
            (
                Tool::DoubleTap,
                json!({"expected_revision":7,"at":{"x":12.5,"y":90}}),
                json!({"type":"double_tap","at":{"x":12.5,"y":90.0}}),
            ),
            (
                Tool::LongPress,
                json!({"expected_revision":7,"at":{"x":12.5,"y":90},"duration_ms":750}),
                json!({"type":"long_press","at":{"x":12.5,"y":90.0},"duration_ms":750}),
            ),
            (
                Tool::Key,
                json!({"expected_revision":7,"key":"backspace"}),
                json!({"type":"key","key":"backspace"}),
            ),
            (
                Tool::Tap,
                json!({"expected_revision":7,"at":{"x":12.5,"y":900}}),
                json!({"type":"tap","at":{"x":12.5,"y":900.0}}),
            ),
            (
                Tool::Swipe,
                json!({"expected_revision":7,"from":{"x":1,"y":2},"to":{"x":3,"y":4},"duration_ms":80}),
                json!({"type":"swipe","from":{"x":1.0,"y":2.0},"to":{"x":3.0,"y":4.0},"duration_ms":80}),
            ),
            (
                Tool::Text,
                json!({"expected_revision":7,"value":"hello café"}),
                json!({"type":"text","value":"hello café"}),
            ),
            (
                Tool::LaunchApp,
                json!({"expected_revision":7,"bundle_id":"com.example.app"}),
                json!({"type":"launch_app","bundle_id":"com.example.app"}),
            ),
        ] {
            let (revision, action) = parse(tool, arguments).unwrap();
            assert_eq!(revision, Revision(7));
            assert_eq!(serde_json::to_value(action).unwrap(), expected);
        }
    }

    #[test]
    fn malformed_interaction_has_no_action() {
        for (tool, arguments) in [
            (Tool::Key, json!({"expected_revision":1,"key":"command"})),
            (
                Tool::Key,
                json!({"expected_revision":1,"key":"enter","value":"extra"}),
            ),
            (
                Tool::LongPress,
                json!({"expected_revision":1,"at":{"x":1,"y":2},"duration_ms":0}),
            ),
            (
                Tool::LongPress,
                json!({"expected_revision":1,"at":{"x":1,"y":2},"duration_ms":5001}),
            ),
            (
                Tool::DoubleTap,
                json!({"expected_revision":1,"at":{"x":-1,"y":2}}),
            ),
            (
                Tool::Tap,
                json!({"expected_revision":1,"at":{"x":-1,"y":2}}),
            ),
            (
                Tool::Tap,
                json!({"expected_revision":1,"at":{"x":1,"y":2,"z":3}}),
            ),
            (
                Tool::Tap,
                json!({"expected_revision":-1,"at":{"x":1,"y":2}}),
            ),
            (
                Tool::Swipe,
                json!({"expected_revision":1,"from":{"x":1,"y":2},"to":{"x":3,"y":4},"duration_ms":0}),
            ),
            (
                Tool::Swipe,
                json!({"expected_revision":1,"from":{"x":1,"y":2},"to":{"x":3,"y":4},"duration_ms":5001}),
            ),
            (Tool::Text, json!({"expected_revision":1,"value":"a\nb"})),
            (Tool::Text, json!({"expected_revision":1,"value":""})),
            (
                Tool::Text,
                json!({"expected_revision":1,"value":"a".repeat(4097)}),
            ),
            (
                Tool::LaunchApp,
                json!({"expected_revision":1,"bundle_id":"https://example.com"}),
            ),
            (
                Tool::LaunchApp,
                json!({"expected_revision":1,"bundle_id":"com..app"}),
            ),
        ] {
            assert!(parse(tool, arguments).is_err());
        }
    }

    #[test]
    fn nested_duplicate_fields_are_rejected_in_raw_frame() {
        assert!(!valid_raw(
            Tool::Tap,
            r#"{"expected_revision":1,"at":{"x":2,"x":3,"y":4}}"#
        ));
        assert!(!valid_raw(
            Tool::Swipe,
            r#"{"expected_revision":1,"from":{"x":2,"y":4},"to":{"x":3,"y":4},"duration_ms":80,"duration_ms":90}"#
        ));
        assert!(!valid_raw(
            Tool::Text,
            r#"{"expected_revision":1,"value":"a","value":"b"}"#
        ));
        assert!(!valid_raw(
            Tool::LaunchApp,
            r#"{"expected_revision":1,"bundle_id":"a.b","bundle_id":"c.d"}"#
        ));
    }
}
