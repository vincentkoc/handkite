use super::MAX_RESPONSE;
use crate::wire::{Reply, WireObservation};
use handkite_core::protocol::ImageFormat;
use serde_json::{Value, json};
use std::io;

pub(super) fn exchange_reply<F>(request: Value, exchange: &mut F) -> Result<Reply, ()>
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    let request = serde_json::to_vec(&request).map_err(|_| ())?;
    let bytes = exchange(&request).map_err(|_| ())?;
    if bytes.len() > MAX_RESPONSE {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

pub(super) fn metadata(observation: &WireObservation) -> Value {
    json!({"device": observation.device, "revision": observation.revision,
        "window": observation.window, "state": observation.state})
}

pub(super) fn text_result(value: Value) -> Value {
    json!({"content": [{"type": "text", "text": value.to_string()}]})
}

pub(super) fn with_service_code(mut result: Value, code: &str) -> Value {
    if matches!(
        code,
        "Busy"
            | "Randomness"
            | "DeviceMismatch"
            | "StaleObservation"
            | "InvalidObservationWindow"
            | "RevisionExhausted"
            | "Adapter"
            | "Poisoned"
            | "InvalidOwner"
            | "InvalidLease"
            | "StaleRevision"
            | "ViewChanged"
            | "LockedAction"
    ) {
        result["structuredContent"] = json!({"code":code});
    }
    result
}

pub(super) fn observation_content(observation: WireObservation) -> Vec<Value> {
    selected_observation_content(observation, false, true)
}

pub(super) fn compact_observation_content(observation: WireObservation) -> Vec<Value> {
    selected_observation_content(observation, true, true)
}

pub(super) fn selected_observation_content(
    observation: WireObservation,
    compact: bool,
    include_image: bool,
) -> Vec<Value> {
    let metadata = metadata(&observation);
    let mut payload = if compact {
        let (tree, format, reason) = match super::compact::project(&observation.accessibility_tree)
        {
            Ok(tree) => (tree, "compact", "requested"),
            Err(reason) => (observation.accessibility_tree, "full", reason.reason()),
        };
        json!({"metadata":metadata, "accessibility_tree":tree,
            "tree_format":format, "tree_format_version":1, "tree_format_reason":reason})
    } else {
        json!({"metadata":metadata, "accessibility_tree":observation.accessibility_tree})
    };
    if !include_image {
        payload["image"] = json!({"included":false,"reason":"requested"});
    }
    let mut content = vec![json!({"type":"text", "text":payload.to_string()})];
    if include_image {
        let mime = match observation.screenshot_format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        };
        content
            .push(json!({"type":"image", "data":observation.screenshot_base64, "mimeType":mime}));
    }
    content
}
