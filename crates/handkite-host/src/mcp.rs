use handkite_core::protocol::DeviceKey;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub fn serve<R, W, F>(
    mut input: R,
    mut output: W,
    device: &DeviceKey,
    mut exchange: F,
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
        let request: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let response = if request["method"] == "tools/call" && !valid_tool_frame(&line) {
            request
                .get("id")
                .map(|id| error(id.clone(), -32602, "invalid tool call"))
        } else {
            process(&request, device, &mut exchange)
        };
        if let Some(response) = response {
            let bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
            if bytes.len() > MAX_RESPONSE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCP response too large",
                ));
            }
            output.write_all(&bytes)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn process<F>(request: &Value, device: &DeviceKey, exchange: &mut F) -> Option<Value>
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
        Some("tools/list") => json!({"tools": [
            {
                "name": "handkite_status",
                "description": "Observe the pinned iPhone and return lock state and capture metadata without screen content.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
            },
            {
                "name": "handkite_observe",
                "description": "Capture one paired screenshot and accessibility tree from the pinned iPhone. Screen content is private.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
            }
        ]}),
        Some("tools/call") => {
            let Ok(params) = serde_json::from_value::<ToolCall>(request["params"].clone()) else {
                return Some(error(id, -32602, "invalid tool call"));
            };
            if !params
                .arguments
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                return Some(error(id, -32602, "tool arguments must be empty"));
            }
            match params.name.as_str() {
                "handkite_status" | "handkite_observe" => {
                    tool_result(&params.name, device, exchange)
                }
                _ => return Some(error(id, -32602, "unknown tool")),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionArguments {
    #[serde(rename = "expected_revision")]
    _revision: handkite_core::protocol::Revision,
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
        #[serde(borrow)]
        arguments: Option<&'a serde_json::value::RawValue>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}

    let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(bytes) else {
        return false;
    };
    let arguments = envelope.params.arguments.map_or("{}", |raw| raw.get());
    if matches!(
        envelope.params.name.as_str(),
        "handkite_home" | "handkite_launch_clock"
    ) {
        serde_json::from_str::<RevisionArguments>(arguments).is_ok()
    } else {
        serde_json::from_str::<Empty>(arguments).is_ok()
    }
}

fn empty_arguments() -> Value {
    json!({})
}

fn tool_result<F>(name: &str, device: &DeviceKey, exchange: &mut F) -> Value
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let request = json!({"method": "observe", "device": device});
    let request = match serde_json::to_vec(&request) {
        Ok(request) => request,
        Err(_) => return tool_error("request encoding failed"),
    };
    let response = match exchange(&request) {
        Ok(response) => response,
        Err(_) => return tool_error("device service unavailable"),
    };
    let response: Value = match serde_json::from_slice(&response) {
        Ok(response) => response,
        Err(_) => return tool_error("invalid device response"),
    };
    if response.get("type") != Some(&json!("observed")) {
        return tool_error("device observation failed");
    }
    let observation = &response["observation"];
    if observation["device"] != json!(device) {
        return tool_error("device identity mismatch");
    }
    let metadata = json!({
        "device": device,
        "revision": observation["revision"],
        "window": observation["window"],
        "state": observation["state"]
    });
    if name == "handkite_status" {
        return json!({"content": [{"type": "text", "text": metadata.to_string()}]});
    }
    let Some(tree) = observation["accessibility_tree"].as_str() else {
        return tool_error("accessibility tree unavailable");
    };
    let Some(image) = observation["screenshot_base64"].as_str() else {
        return tool_error("screenshot unavailable");
    };
    let mime = match observation["screenshot_format"].as_str() {
        Some("png") => "image/png",
        Some("jpeg") => "image/jpeg",
        _ => return tool_error("unsupported screenshot format"),
    };
    let text = json!({"metadata": metadata, "accessibility_tree": tree}).to_string();
    json!({"content": [
        {"type": "text", "text": text},
        {"type": "image", "data": image, "mimeType": mime}
    ]})
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
                process(&request, &device, &mut exchange).unwrap()["error"]["code"],
                -32602
            );
        }
        let mut request = valid.clone();
        request["params"]["device"] = json!("other");
        assert_eq!(
            process(&request, &device, &mut exchange).unwrap()["error"]["code"],
            -32602
        );
        let mut request = valid.clone();
        request["socket"] = json!("/other");
        assert_eq!(
            process(&request, &device, &mut exchange).unwrap()["error"]["code"],
            -32600
        );
        for id in [Value::Null, json!({}), json!(true), json!(1.5)] {
            let mut request = valid.clone();
            request["id"] = id;
            assert_eq!(
                process(&request, &device, &mut exchange).unwrap()["error"]["code"],
                -32600
            );
        }
    }
    #[test]
    fn duplicate_fields_are_rejected_before_socket_dispatch() {
        for input in [
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":1,"expected_revision":2}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_status","name":"handkite_acquire_control","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"tools/call","params":{"name":"handkite_acquire_control","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_acquire_control","arguments":{},"arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":18446744073709551616}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"handkite_home","arguments":{"expected_revision":1.0}}}"#,
        ] {
            let mut output = Vec::new();
            serve(
                Cursor::new(format!("{input}\n")),
                &mut output,
                &DeviceKey::new("phone").unwrap(),
                |_| panic!("duplicate field reached socket"),
            )
            .unwrap();
            let reply: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(reply["error"]["code"], -32602);
        }
    }
}
