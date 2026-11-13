use crate::wire::Reply;
use handkite_core::protocol::DeviceKey;
#[cfg(test)]
use handkite_core::protocol::{Action, Revision};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

mod catalog;
mod compact;
mod interaction;
mod response;
mod session;
pub use catalog::Capabilities;
use catalog::Tool;
use response::{exchange_reply, metadata};
use session::Session;

#[cfg(test)]
mod navigation_tests;
#[cfg(test)]
mod presentation_tests;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub fn serve<R, W, F>(input: R, output: W, device: &DeviceKey, exchange: F) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    serve_with_capabilities(input, output, device, Capabilities::Observation, exchange)
}

pub fn serve_with_capabilities<R, W, F>(
    mut input: R,
    mut output: W,
    device: &DeviceKey,
    capabilities: Capabilities,
    mut exchange: F,
) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let mut session = Session::default();
    let result = serve_loop(
        &mut input,
        &mut output,
        device,
        capabilities,
        &mut session,
        &mut exchange,
    );
    // This also runs after a partial frame, failed read, or failed output write.
    // A killed process cannot clean up; the daemon's fixed TTL remains the bound.
    session.cleanup(&mut exchange);
    result
}

fn serve_loop<R, W, F>(
    input: &mut R,
    output: &mut W,
    device: &DeviceKey,
    capabilities: Capabilities,
    session: &mut Session,
    exchange: &mut F,
) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    loop {
        let mut line = Vec::new();
        loop {
            let available = input.fill_buf()?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial MCP frame",
                ));
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if line.len() + count > MAX_REQUEST {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCP frame too large",
                ));
            }
            let ended = available[count - 1] == b'\n';
            line.extend_from_slice(&available[..count]);
            input.consume(count);
            if ended {
                break;
            }
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
            Ok(request) if request["method"] == "tools/call" && !valid_tool_frame(&line) => request
                .get("id")
                .map(|id| error(id.clone(), -32602, "invalid tool call")),
            Ok(request) => process(&request, device, capabilities, session, exchange),
        };
        if let Some(response) = response {
            let bytes = encode_response(response)?;
            output.write_all(&bytes)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn encode_response(mut response: Value) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() <= MAX_RESPONSE {
        return Ok(bytes);
    }
    let result = &mut response["result"];
    if let Some(content) = result["content"]
        .as_array_mut()
        .filter(|items| items.len() == 3)
    {
        // Action replies put the receipt first, followed by the optional tree/image.
        // Preserve known delivery even when that screen cannot fit in one frame.
        content.truncate(1);
        result["isError"] = json!(true);
    } else {
        *result = tool_error("ResponseTooLarge: observation exceeds the MCP frame limit");
    }
    result["structuredContent"] = json!({"code":"ResponseTooLarge"});
    let bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP response too large",
        ));
    }
    Ok(bytes)
}

fn process<F>(
    request: &Value,
    device: &DeviceKey,
    capabilities: Capabilities,
    session: &mut Session,
    exchange: &mut F,
) -> Option<Value>
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let id = request.get("id")?.clone();
    if request.get("jsonrpc") != Some(&json!("2.0"))
        || !(id.is_string() || id.is_i64() || id.is_u64())
        || request.as_object().is_none_or(|fields| {
            fields
                .keys()
                .any(|key| !matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params"))
        })
    {
        return Some(error(id, -32600, "invalid JSON-RPC request"));
    }
    let result = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "handkite", "version": env!("CARGO_PKG_VERSION")}
        }),
        Some("ping") => json!({}),
        Some("tools/list") => catalog::catalog(capabilities),
        Some("tools/call") => {
            let Ok(params) = serde_json::from_value::<ToolCall>(request["params"].clone()) else {
                return Some(error(id, -32602, "invalid tool call"));
            };
            let Some(tool) =
                Tool::from_name(&params.name).filter(|tool| tool.allowed(capabilities))
            else {
                return Some(error(id, -32602, "unknown or unauthorized tool"));
            };
            if tool.is_action() {
                let Ok((revision, action)) = interaction::parse(tool, params.arguments) else {
                    return Some(error(id, -32602, "invalid action arguments"));
                };
                session.act(
                    device,
                    revision,
                    action,
                    Tool::Observe.allowed(capabilities),
                    exchange,
                )
            } else {
                let compact = if tool == Tool::Observe {
                    if !params.arguments.is_object() {
                        return Some(error(id, -32602, "invalid observation arguments"));
                    }
                    let Ok(args) = serde_json::from_value::<ObserveArguments>(params.arguments)
                    else {
                        return Some(error(id, -32602, "invalid observation arguments"));
                    };
                    args.tree_format
                } else if !params
                    .arguments
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return Some(error(id, -32602, "tool arguments must be empty"));
                } else {
                    false
                };
                match tool {
                    Tool::Status | Tool::Observe => tool_result(tool, compact, device, exchange),
                    Tool::Acquire => session.acquire(exchange),
                    Tool::Release => session.release(exchange),
                    _ => unreachable!(),
                }
            }
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveArguments {
    #[serde(default, deserialize_with = "compact_only")]
    tree_format: bool,
}

fn compact_only<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value == "compact" {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("invalid tree format"))
    }
}

fn present_arguments<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<&'de serde_json::value::RawValue>, D::Error> {
    // Missing arguments default to {}; an explicit null must remain visible.
    <&serde_json::value::RawValue>::deserialize(deserializer).map(Some)
}

// Validate the raw frame as well: decoding to Value alone discards duplicate keys.
fn valid_tool_frame(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope<'a> {
        #[serde(rename = "jsonrpc")]
        _jsonrpc: String,
        #[serde(rename = "id")]
        _id: Value,
        #[serde(rename = "method")]
        _method: String,
        #[serde(borrow)]
        params: Arguments<'a>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arguments<'a> {
        name: String,
        #[serde(borrow, default, deserialize_with = "present_arguments")]
        arguments: Option<&'a serde_json::value::RawValue>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}

    let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(bytes) else {
        return false;
    };
    let arguments = envelope.params.arguments.map_or("{}", |raw| raw.get());
    match Tool::from_name(&envelope.params.name) {
        Some(tool) if tool.is_action() => interaction::valid_raw(tool, arguments),
        Some(Tool::Observe) => {
            arguments.trim_start().starts_with('{')
                && serde_json::from_str::<ObserveArguments>(arguments).is_ok()
        }
        _ => serde_json::from_str::<Empty>(arguments).is_ok(),
    }
}

fn empty_arguments() -> Value {
    json!({})
}

fn tool_result<F>(tool: Tool, compact: bool, device: &DeviceKey, exchange: &mut F) -> Value
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let observation = match exchange_reply(json!({"method": "observe", "device": device}), exchange)
    {
        Ok(Reply::Observed { observation }) => observation,
        Ok(Reply::Error { code }) => {
            return response::with_service_code(
                tool_error("device observation unavailable"),
                &code,
            );
        }
        _ => return tool_error("device observation unavailable"),
    };
    if &observation.device != device {
        return response::with_service_code(
            tool_error("device identity mismatch"),
            "DeviceMismatch",
        );
    }
    let metadata = metadata(&observation);
    if tool == Tool::Status {
        return response::text_result(metadata);
    }
    let content = if compact {
        response::compact_observation_content(observation)
    } else {
        response::observation_content(observation)
    };
    json!({"content": content})
}

fn tool_error(message: &str) -> Value {
    json!({"isError": true, "content": [{"type": "text", "text": message}]})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(input: &str, response: &str) -> Vec<Value> {
        let mut output = Vec::new();
        let device = DeviceKey::new("phone").unwrap();
        serve(
            Cursor::new(input.as_bytes()),
            &mut output,
            &device,
            |request| {
                let request: Value = serde_json::from_slice(request).unwrap();
                assert_eq!(request, json!({"method": "observe", "device": "phone"}));
                Ok(response.as_bytes().to_vec())
            },
        )
        .unwrap();
        output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn handshake_and_catalog_are_read_only() {
        let replies = run(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "",
        );
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
        let tools = replies[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(
            tools
                .iter()
                .all(|tool| tool["name"].as_str().unwrap().starts_with("handkite_"))
        );
    }

    #[test]
    fn malformed_json_returns_parse_error_and_preserves_the_session() {
        let replies = run(
            "{broken private payload\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "",
        );
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], error(Value::Null, -32700, "parse error"));
        assert_eq!(replies[1]["id"], 2);
        assert!(replies[1]["result"]["tools"].is_array());
    }

    #[test]
    fn observation_errors_expose_only_fixed_codes() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n";
        for code in [
            "Adapter",
            "DeviceMismatch",
            "StaleObservation",
            "InvalidObservationWindow",
            "private detail",
        ] {
            let replies = run(input, &json!({"type":"error","code":code}).to_string());
            assert_eq!(replies[0]["result"]["isError"], true);
            if code == "private detail" {
                assert!(replies[0]["result"].get("structuredContent").is_none());
                assert!(!replies[0].to_string().contains(code));
            } else {
                assert_eq!(replies[0]["result"]["structuredContent"]["code"], code);
            }
        }
    }

    #[test]
    fn one_observation_yields_metadata_tree_and_image() {
        let observed = r#"{"type":"observed","observation":{"device":"phone","revision":3,"window":{"started_unix_ms":1,"finished_unix_ms":2},"state":{"lock":"locked","wda_ready":true,"foreground_app":null},"accessibility_tree":"<screen/>","screenshot_format":"png","screenshot_base64":"AQID"}}"#;
        let replies = run(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_observe\",\"arguments\":{}}}\n",
            observed,
        );
        assert_eq!(replies[0]["result"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(replies[1]["result"]["content"][1]["data"], "AQID");
        assert!(
            replies[1]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("<screen/>")
        );
    }

    #[test]
    fn no_action_or_cross_device_call_is_dispatched() {
        let replies = run(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_act\",\"arguments\":{}}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_observe\",\"arguments\":{\"device\":\"other\"}}}\n",
            "",
        );
        assert_eq!(replies[0]["error"]["code"], -32602);
        assert_eq!(replies[1]["error"]["code"], -32602);
    }

    #[test]
    fn forged_envelopes_and_arguments_never_reach_the_socket() {
        let device = DeviceKey::new("phone").unwrap();
        let mut exchange = |_: &[u8]| panic!("invalid input reached the socket");
        let valid = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "handkite_status", "arguments": {}}});
        for arguments in [
            Value::Null,
            json!([]),
            json!(1),
            json!({"device": "other"}),
            json!({"owner": "other"}),
            json!({"lease": "forged"}),
            json!({"socket": "/other"}),
        ] {
            let mut request = valid.clone();
            request["params"]["arguments"] = arguments;
            assert_eq!(
                process(
                    &request,
                    &device,
                    Capabilities::Observation,
                    &mut Session::default(),
                    &mut exchange
                )
                .unwrap()["error"]["code"],
                -32602
            );
        }
        let mut request = valid.clone();
        request["params"]["device"] = json!("other");
        assert_eq!(
            process(
                &request,
                &device,
                Capabilities::Observation,
                &mut Session::default(),
                &mut exchange
            )
            .unwrap()["error"]["code"],
            -32602
        );
        let mut request = valid.clone();
        request["socket"] = json!("/other");
        assert_eq!(
            process(
                &request,
                &device,
                Capabilities::Observation,
                &mut Session::default(),
                &mut exchange
            )
            .unwrap()["error"]["code"],
            -32600
        );
        for id in [Value::Null, json!({}), json!(true), json!(1.5)] {
            let mut request = valid.clone();
            request["id"] = id;
            assert_eq!(
                process(
                    &request,
                    &device,
                    Capabilities::Observation,
                    &mut Session::default(),
                    &mut exchange
                )
                .unwrap()["error"]["code"],
                -32600
            );
        }
    }
    #[test]
    fn status_projects_typed_metadata_without_response_extensions() {
        let observed = r#"{"type":"observed","lease":"private-token","observation":{"device":"phone","revision":3,"window":{"started_unix_ms":1,"finished_unix_ms":2,"secret":"private-token"},"state":{"lock":"locked","wda_ready":true,"foreground_app":null,"secret":"private-token"},"accessibility_tree":"<private-screen/>","screenshot_format":"png","screenshot_base64":"AQID"}}"#;
        let replies = run(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n",
            observed,
        );
        let output = replies[0].to_string();
        assert!(!output.contains("private-token"));
        assert!(!output.contains("private-screen"));
        assert!(!output.contains("AQID"));
    }
}
