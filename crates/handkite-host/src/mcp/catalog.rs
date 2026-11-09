use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tool {
    Status,
    Observe,
}

impl Tool {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "handkite_status" => Some(Self::Status),
            "handkite_observe" => Some(Self::Observe),
            _ => None,
        }
    }
}

pub(super) fn catalog() -> Value {
    let definitions = [
        (
            "handkite_status",
            "Observe the pinned iPhone and return lock state and capture metadata without screen content.",
        ),
        (
            "handkite_observe",
            "Capture one paired screenshot and accessibility tree from the pinned iPhone. Screen content is private.",
        ),
    ];
    let tools: Vec<_> = definitions
        .into_iter()
        .map(|(name, description)| {
            json!({
                "name": name, "description": description,
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
            })
        })
        .collect();
    json!({"tools": tools})
}
