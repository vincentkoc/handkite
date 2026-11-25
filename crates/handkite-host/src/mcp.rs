use crate::wire::Reply;
use handkite_core::protocol::DeviceKey;
#[cfg(test)]
use handkite_core::protocol::{Action, Revision};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

mod alert_tools;
mod catalog;
mod compact;
mod elements;
mod interaction;
mod outline;
mod presentation;
mod response;
mod session;
mod target_tools;
pub use catalog::Capabilities;
use catalog::Tool;
use response::exchange_reply;
use session::Session;

#[cfg(test)]
mod home_tests;
#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod navigation_tests;
#[cfg(test)]
mod outline_tests;
#[cfg(test)]
mod pacing_tests;
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
    input: R,
    output: W,
    device: &DeviceKey,
    capabilities: Capabilities,
    exchange: F,
) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    serve_with_pacing(
        input,
        output,
        device,
        capabilities,
        crate::pacing::Pacer::default(),
        exchange,
    )
}

/// As `serve_with_capabilities`, with a session pacing policy chosen at launch.
pub fn serve_with_pacing<R, W, F>(
    mut input: R,
    mut output: W,
    device: &DeviceKey,
    capabilities: Capabilities,
    pacing: crate::pacing::Pacer,
    mut exchange: F,
) -> io::Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let mut session = Session::with_pacing(pacing);
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
        let _operation = crate::timing::Span::operation("mcp_request");
        let response = match serde_json::from_slice::<Value>(&line) {
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
            Ok(request) if request["method"] == "tools/call" && !valid_tool_frame(&line) => request
                .get("id")
                .map(|id| error(id.clone(), -32602, "invalid tool call")),
            Ok(request) => process(&request, device, capabilities, session, exchange),
        };
        if let Some(response) = response {
            let (bytes, screen_kept) = encode_response(response)?;
            if !screen_kept {
                session.presentation.clear();
            }
            let _timing = crate::timing::Span::new("mcp_write");
            output.write_all(&bytes)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn encode_response(mut response: Value) -> io::Result<(Vec<u8>, bool)> {
    let _timing = crate::timing::Span::new("mcp_serialization");
    let bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() <= MAX_RESPONSE {
        return Ok((bytes, true));
    }
    let result = &mut response["result"];
    if let Some(content) = result["content"].as_array_mut().filter(|items| {
        items.len() >= 2
            && items.first().is_some_and(|item| {
                item["text"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .is_some_and(|payload| payload.get("receipt").is_some())
            })
    }) {
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
    Ok((bytes, false))
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
            if !matches!(tool, Tool::Observe | Tool::Status) {
                session.home_observe_hint = false;
            }
            if matches!(tool, Tool::ObserveAlert | Tool::TapAlert) {
                alert_tools::call(tool, params.arguments, device, session, exchange)
            } else if matches!(tool, Tool::TapElement | Tool::TextElement) {
                // Gap and rate cap only: native target geometry stays daemon-owned.
                if session.lease.is_some() {
                    session.pacing.begin(None);
                }
                session.act_element(
                    tool == Tool::TextElement,
                    params.arguments,
                    device,
                    exchange,
                )
            } else if tool.is_action() {
                let mut arguments = params.arguments;
                let Ok(pacing) = crate::pacing::Override::take(&mut arguments) else {
                    return Some(error(id, -32602, "invalid action arguments"));
                };
                let Ok((revision, action, options)) = interaction::parse_response(tool, arguments)
                else {
                    return Some(error(id, -32602, "invalid action arguments"));
                };
                let action = session.pace(pacing, revision, action);
                session.act(
                    device,
                    revision,
                    action,
                    Tool::Observe.allowed(capabilities),
                    options,
                    exchange,
                )
            } else {
                let options = if tool == Tool::Observe {
                    if !params.arguments.is_object() {
                        return Some(error(id, -32602, "invalid observation arguments"));
                    }
                    let Ok(args) = serde_json::from_value::<ObserveArguments>(params.arguments)
                    else {
                        return Some(error(id, -32602, "invalid observation arguments"));
                    };
                    args
                } else if !params
                    .arguments
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return Some(error(id, -32602, "tool arguments must be empty"));
                } else {
                    presentation::Options::default()
                };
                match tool {
                    Tool::Status | Tool::Observe => {
                        tool_result(tool, options, device, session, exchange)
                    }
                    Tool::Acquire => session.acquire(exchange),
                    Tool::Release => session.release(exchange),
                    _ => unreachable!(),
                }
            }
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    if result.get("isError") == Some(&json!(true)) {
        session.presentation.clear();
    }
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

type ObserveArguments = presentation::Options;

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
        Some(tool @ (Tool::ObserveAlert | Tool::TapAlert)) => {
            alert_tools::valid_raw(tool, arguments)
        }
        Some(Tool::TapElement) => target_tools::valid_raw(false, arguments),
        Some(Tool::TextElement) => target_tools::valid_raw(true, arguments),
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

fn tool_result<F>(
    tool: Tool,
    mut options: presentation::Options,
    device: &DeviceKey,
    session: &mut Session,
    exchange: &mut F,
) -> Value
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    if tool == Tool::Observe
        && options.capture == handkite_core::protocol::CaptureMode::Paired
        && session.home_observe_hint
        && let Some(result) = observe_known_home(options, device, session, exchange)
    {
        return result;
    }
    if tool == Tool::Status {
        session.home_observe_hint = false;
    }
    let method = if tool == Tool::Status {
        "status"
    } else if options.targets {
        "observe_targets"
    } else if options.capture == handkite_core::protocol::CaptureMode::ImageOnly {
        "observe_image"
    } else if options.tree_format == presentation::TreeFormat::Outline {
        "observe_outline"
    } else {
        "observe"
    };
    let mut native_targets = None;
    let mut navigation_only = false;
    let mut request = json!({"method": method, "device": device});
    if method == "observe_targets"
        && let Some(revision) = options.since_revision
        && session.presentation.has_element_baseline(revision)
        && session
            .target_inventory
            .as_ref()
            .is_some_and(|b| b.0 == revision)
    {
        request["since_revision"] = json!(revision);
    }
    let observation = match exchange_reply(request, exchange) {
        Ok(Reply::ObservedTargets {
            observation,
            targets,
        }) if method == "observe_targets" => {
            native_targets = Some(targets);
            observation
        }
        Ok(Reply::ObservedImage { observation }) if method == "observe_image" => {
            session.presentation.clear();
            if &observation.device != device {
                return response::with_service_code(
                    tool_error("device identity mismatch"),
                    "DeviceMismatch",
                );
            }
            return json!({"content":response::image_content(observation)});
        }
        Ok(Reply::Status { status }) if tool == Tool::Status => {
            if &status.device != device {
                return response::with_service_code(
                    tool_error("device identity mismatch"),
                    "DeviceMismatch",
                );
            }
            session.home_observe_hint = is_unlocked_home(&status.state);
            let mut payload = json!(status);
            payload["revision_scope"] = json!("navigation");
            payload["allowed_actions"] = json!(["home", "launch_app"]);
            return response::text_result(payload);
        }
        Ok(Reply::ObservedOutline {
            observation,
            navigation_only: limited,
        }) if method == "observe_outline" => {
            navigation_only = limited;
            observation
        }
        Ok(Reply::Observed { observation }) if method == "observe" => observation,
        Ok(Reply::Error { code })
            if matches!(method, "observe" | "observe_outline") && code == "AdapterCaptureHome" =>
        {
            return navigation_fallback(
                device,
                session,
                exchange,
                "home_navigation_only",
                options.image,
            );
        }
        Ok(Reply::Error { code })
            if matches!(method, "observe" | "observe_outline" | "observe_targets")
                && code == "AdapterCaptureImageDrift" =>
        {
            session.presentation.clear();
            // A drifting full pair may still have locally stable XML/OCR targets.
            // This is one read-only fallback, never a retry of an input.
            if method == "observe" && options.tree_format == presentation::TreeFormat::Elements {
                let mut target_options = options;
                target_options.targets = true;
                let result = tool_result(Tool::Observe, target_options, device, session, exchange);
                return result;
            }
            return navigation_fallback(
                device,
                session,
                exchange,
                "capture_image_drift",
                options.image,
            );
        }

        Ok(Reply::Error { code }) if method == "observe_targets" && code == "AdapterTargetHome" => {
            return navigation_fallback(
                device,
                session,
                exchange,
                "home_navigation_only",
                options.image,
            );
        }
        Ok(Reply::Error { code }) => {
            return response::with_service_code(
                tool_error(if method == "observe_targets" {
                    "native targets unavailable; request image-only observation (capture=image_only, tree_format=omit)"
                } else {
                    "device observation unavailable"
                }),
                &code,
            );
        }
        _ => {
            return tool_error(if method == "observe_targets" {
                "native targets unavailable; request image-only observation (capture=image_only, tree_format=omit)"
            } else {
                "device observation unavailable"
            });
        }
    };
    if &observation.device != device {
        return response::with_service_code(
            tool_error("device identity mismatch"),
            "DeviceMismatch",
        );
    }
    if matches!(method, "observe" | "observe_outline")
        && observation.state.foreground_app.as_deref() == Some("com.apple.springboard")
        && observation.state.lock == handkite_core::protocol::LockState::Uncertain
    {
        return navigation_fallback(
            device,
            session,
            exchange,
            "home_navigation_only",
            options.image,
        );
    }
    if native_targets.as_ref().is_some_and(Vec::is_empty) {
        options.image = presentation::ImageMode::Always;
    }
    let revision = observation.revision;
    let state = observation.state.clone();
    let native_regions = observation
        .accessibility_tree
        .starts_with("<AppiumAUT source=\"native_regions\">");
    if navigation_only {
        session.presentation.clear();
        session.target_inventory = None;
        session.home_observe_hint = false;
    }
    let content = session.presentation.content(observation, options);
    let mut result = json!({"content": content});
    if navigation_only {
        let mut payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        payload["revision"] = json!(revision);
        payload["revision_scope"] = json!("navigation");
        payload["metadata"]["revision_scope"] = json!("navigation");
        payload["tree_scope"] = json!("presentation");
        payload["visibility"] = json!("unknown");
        payload["allowed_actions"] = json!(["home", "launch_app"]);
        payload["target_actionable"] = json!(false);
        result["content"][0]["text"] = json!(payload.to_string());
        result["structuredContent"] = payload;
    }
    if options.tree_format == presentation::TreeFormat::Elements {
        result["structuredContent"] =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    }
    if let Some(targets) = native_targets {
        let mut payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        payload["revision"] = json!(revision);
        payload["target_only"] = json!(true);
        payload["metadata"]["target_only"] = json!(true);
        payload["target_actionable"] = json!(!targets.is_empty());
        if targets.is_empty() {
            payload["target_reason"] = json!("no_stable_native_targets");
        }
        if native_regions {
            payload["tree_scope"] = json!("native_regions");
        }
        let previous = session
            .target_inventory
            .take()
            .filter(|b| Some(b.0) == options.since_revision && b.1 == state);
        if let Some((base, _, old)) = previous.filter(|_| payload.get("elements_delta").is_some()) {
            let old_ids: std::collections::BTreeMap<_, _> =
                old.iter().map(|t| (&t.target_id, t)).collect();
            let new_ids: std::collections::BTreeMap<_, _> =
                targets.iter().map(|t| (&t.target_id, t)).collect();
            let removed: Vec<_> = old
                .iter()
                .filter(|t| !new_ids.contains_key(&t.target_id))
                .map(|t| &t.target_id)
                .collect();
            let added: Vec<_> = targets
                .iter()
                .filter(|t| !old_ids.contains_key(&t.target_id))
                .collect();
            let changed: Vec<_> = targets
                .iter()
                .filter(|t| {
                    old_ids.get(&t.target_id).is_some_and(|old| {
                        serde_json::to_value(old).unwrap() != serde_json::to_value(t).unwrap()
                    })
                })
                .collect();
            payload["targets_delta"] =
                json!({"base_revision":base,"added":added,"removed":removed,"changed":changed});
        } else {
            payload["targets"] = json!(targets);
        }
        session.target_inventory = Some((revision, state, targets));
        result["structuredContent"] = payload.clone();
        result["content"][0]["text"] = json!(payload.to_string());
    }
    result
}

fn navigation_fallback<F>(
    device: &DeviceKey,
    session: &mut Session,
    exchange: &mut F,
    reason: &str,
    image: presentation::ImageMode,
) -> Value
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    session.presentation.clear();
    session.target_inventory = None;
    match exchange_reply(
        json!({"method":"observe_navigation","device":device}),
        exchange,
    ) {
        Ok(Reply::ObservedNavigation { observation }) if &observation.device == device => {
            session.home_observe_hint = is_unlocked_home(&observation.state);
            let content = response::navigation_content(
                observation,
                reason,
                image != presentation::ImageMode::Omit,
            );
            let payload: Value =
                serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
            json!({"content":content,"structuredContent":payload})
        }
        Ok(Reply::ObservedNavigation { .. }) => {
            response::with_service_code(tool_error("device identity mismatch"), "DeviceMismatch")
        }
        Ok(Reply::Error { code }) => {
            response::with_service_code(tool_error("device observation unavailable"), &code)
        }
        _ => tool_error("device observation unavailable"),
    }
}

fn is_unlocked_home(state: &handkite_core::protocol::DeviceState) -> bool {
    state.wda_ready
        && state.lock == handkite_core::protocol::LockState::Unlocked
        && state.foreground_app.as_deref() == Some("com.apple.springboard")
}

fn observe_known_home<F>(
    options: presentation::Options,
    device: &DeviceKey,
    session: &mut Session,
    exchange: &mut F,
) -> Option<Value>
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    session.home_observe_hint = false;
    session.presentation.clear();
    session.target_inventory = None;
    match exchange_reply(
        json!({"method":"observe_navigation","device":device}),
        exchange,
    ) {
        Ok(Reply::ObservedNavigation { observation }) if &observation.device == device => {
            if !is_unlocked_home(&observation.state) {
                // The hint cannot suppress a real app or lock-screen tree.
                return None;
            }
            session.home_observe_hint = true;
            let content = response::navigation_content(
                observation,
                "home_navigation_only",
                options.image != presentation::ImageMode::Omit,
            );
            let payload: Value =
                serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
            Some(json!({"content":content,"structuredContent":payload}))
        }
        Ok(Reply::ObservedNavigation { .. }) => Some(response::with_service_code(
            tool_error("device identity mismatch"),
            "DeviceMismatch",
        )),
        Ok(Reply::Error { code }) => Some(response::with_service_code(
            tool_error("device observation unavailable"),
            &code,
        )),
        _ => Some(tool_error("device observation unavailable")),
    }
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
                assert_eq!(request["device"], "phone");
                assert!(matches!(
                    request["method"].as_str(),
                    Some("status" | "observe")
                ));
                assert_eq!(request.as_object().unwrap().len(), 2);
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
        assert_eq!(tools.len(), 3);
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
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_observe\",\"arguments\":{}}}\n",
            observed,
        );
        assert_eq!(replies[0]["result"]["content"].as_array().unwrap().len(), 2);
        assert_eq!(replies[0]["result"]["content"][1]["data"], "AQID");
        assert!(
            replies[0]["result"]["content"][0]["text"]
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
        let observed = r#"{"type":"status","lease":"private-token","status":{"device":"phone","revision":3,"window":{"started_unix_ms":1,"finished_unix_ms":2,"secret":"private-token"},"state":{"lock":"locked","wda_ready":true,"foreground_app":null,"secret":"private-token"},"accessibility_tree":"<private-screen/>","screenshot_base64":"AQID"}}"#;
        let replies = run(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n",
            observed,
        );
        let output = replies[0].to_string();
        assert!(!output.contains("private-token"));
        assert!(!output.contains("private-screen"));
        assert!(!output.contains("AQID"));
    }

    #[test]
    fn status_uses_one_metadata_exchange_with_the_same_public_payload() {
        let device = DeviceKey::new("phone").unwrap();
        let metadata = json!({"device":"phone", "revision":3,
            "window":{"started_unix_ms":1,"finished_unix_ms":2},
            "state":{"lock":"locked","wda_ready":true,"foreground_app":null}});
        let mut calls = 0;
        let mut output = Vec::new();
        serve(
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n"),
            &mut output,
            &device,
            |request| {
                calls += 1;
                assert_eq!(serde_json::from_slice::<Value>(request).unwrap(),
                    json!({"method":"status","device":"phone"}));
                Ok(serde_json::to_vec(&json!({"type":"status","status":metadata})).unwrap())
            },
        ).unwrap();
        assert_eq!(calls, 1);
        let mut metadata = metadata;
        metadata["revision_scope"] = json!("navigation");
        metadata["allowed_actions"] = json!(["home", "launch_app"]);
        let expected = json!({"jsonrpc":"2.0","id":1,
            "result":response::text_result(metadata)});
        assert_eq!(
            output,
            [serde_json::to_vec(&expected).unwrap(), vec![b'\n']].concat()
        );
    }

    #[test]
    fn status_refuses_wrong_or_malformed_replies_without_fallback() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"handkite_status\",\"arguments\":{}}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let metadata = json!({"device":"phone", "revision":3,
            "window":{"started_unix_ms":1,"finished_unix_ms":2},
            "state":{"lock":"locked","wda_ready":true,"foreground_app":null}});
        let mut wrong_device = metadata.clone();
        wrong_device["device"] = json!("other");
        let mut observed = metadata;
        observed["accessibility_tree"] = json!("<private-screen/>");
        observed["screenshot_format"] = json!("png");
        observed["screenshot_base64"] = json!("AQID");
        for reply in [
            json!({"type":"status","status":wrong_device}).to_string(),
            json!({"type":"observed","observation":observed}).to_string(),
            r#"{"type":"status","status":null}"#.into(),
            "private malformed reply".into(),
        ] {
            let mut calls = 0;
            let mut output = Vec::new();
            serve(
                Cursor::new(input),
                &mut output,
                &DeviceKey::new("phone").unwrap(),
                |request| {
                    calls += 1;
                    assert_eq!(
                        serde_json::from_slice::<Value>(request).unwrap(),
                        json!({"method":"status","device":"phone"})
                    );
                    Ok(reply.as_bytes().to_vec())
                },
            )
            .unwrap();
            assert_eq!(calls, 1);
            let replies: Vec<Value> = output
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(replies[0]["result"]["isError"], true);
            assert!(!replies[0].to_string().contains("private"));
            assert_eq!(replies[1]["id"], 2);
            assert!(replies[1]["result"]["tools"].is_array());
        }
    }
}
