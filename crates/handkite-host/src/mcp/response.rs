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

pub(super) fn project_payload(
    observation: &mut WireObservation,
    format: super::presentation::TreeFormat,
) -> Value {
    use super::presentation::TreeFormat;
    let metadata = metadata(observation);
    let original = std::mem::take(&mut observation.accessibility_tree);
    match format {
        TreeFormat::Full => json!({"metadata":metadata,"accessibility_tree":original}),
        TreeFormat::Compact => {
            let (tree, format, reason) = match super::compact::project(&original) {
                Ok(tree) => (tree, "compact", "requested"),
                Err(reason) => (original, "full", reason.reason()),
            };
            json!({"metadata":metadata,"accessibility_tree":tree,"tree_format":format,
                "tree_format_version":1,"tree_format_reason":reason})
        }
        TreeFormat::Outline => match super::outline::project(&original) {
            Ok(outline) => json!({"metadata":metadata,"accessibility_tree":outline.text,
                "tree_format":"outline","tree_format_version":1,"tree_format_reason":"requested",
                "tree_lossy":true,"tree_omissions":{"invisible_nodes":outline.invisible_nodes,"empty_wrappers":outline.empty_wrappers}}),
            Err(reason) => {
                json!({"metadata":metadata,"accessibility_tree":original,"tree_format":"full",
                "tree_format_version":1,"tree_format_reason":reason.reason()})
            }
        },
    }
}

pub(super) fn finish_observation_content(
    mut payload: Value,
    format: ImageFormat,
    image: String,
    include_image: bool,
) -> Vec<Value> {
    if !include_image && payload.get("image").is_none() {
        payload["image"] = json!({"included":false,"reason":"requested"});
    }
    let mut content = vec![json!({"type":"text", "text":payload.to_string()})];
    if include_image {
        let mime = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        };
        content.push(json!({"type":"image", "data":image, "mimeType":mime}));
    }
    content
}
