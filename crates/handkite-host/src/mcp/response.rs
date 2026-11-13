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

pub(super) fn observation_content(observation: WireObservation) -> Vec<Value> {
    let metadata = metadata(&observation);
    let mime = match observation.screenshot_format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
    };
    vec![
        json!({"type": "text", "text": json!({"metadata": metadata,
            "accessibility_tree": observation.accessibility_tree}).to_string()}),
        json!({"type": "image", "data": observation.screenshot_base64, "mimeType": mime}),
    ]
}
