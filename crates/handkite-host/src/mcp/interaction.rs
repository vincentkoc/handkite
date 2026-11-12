use super::catalog::Tool;
use handkite_core::protocol::{Action, Point, Revision};
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
        Tool::Tap => {
            let args: TapArguments = serde_json::from_value(value).map_err(|_| ())?;
            if !args.at.valid() {
                return Err(());
            }
            Ok((
                args.expected_revision,
                Action::Tap {
                    at: args.at.into_point(),
                },
            ))
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
        Tool::Home | Tool::Clock => serde_json::from_str::<RevisionOnly>(raw).is_ok(),
        Tool::Tap => serde_json::from_str::<TapArguments>(raw).is_ok(),
        Tool::Swipe => serde_json::from_str::<SwipeArguments>(raw).is_ok(),
        Tool::Text => serde_json::from_str::<TextArguments>(raw).is_ok(),
        Tool::LaunchApp => serde_json::from_str::<LaunchArguments>(raw).is_ok(),
        _ => false,
    }
}

pub(super) fn schema(tool: Tool) -> Value {
    let revision = json!({"type":"integer","minimum":0,"maximum":u64::MAX});
    let point = json!({"type":"object","properties":{
        "x":{"type":"number","minimum":0,"maximum":10_000},
        "y":{"type":"number","minimum":0,"maximum":10_000}},
        "required":["x","y"],"additionalProperties":false});
    let mut properties = serde_json::Map::new();
    properties.insert("expected_revision".into(), revision);
    let mut required = vec!["expected_revision"];
    match tool {
        Tool::Tap => {
            properties.insert("at".into(), point);
            required.push("at");
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
    fn each_tool_translates_to_one_typed_action() {
        for (tool, arguments, expected) in [
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
