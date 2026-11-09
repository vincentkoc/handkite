use serde_json::{Value, json};

/// Selected by the host launch command, never by MCP request arguments.
#[derive(Clone, Copy, Default)]
pub enum Capabilities {
    #[default]
    Observation,
    Navigation,
    ObservationAndNavigation,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tool {
    Status,
    Observe,
    Acquire,
    Release,
}

impl Tool {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "handkite_status" => Some(Self::Status),
            "handkite_observe" => Some(Self::Observe),
            "handkite_acquire_control" => Some(Self::Acquire),
            "handkite_release_control" => Some(Self::Release),
            _ => None,
        }
    }

    pub(super) fn allowed(self, capabilities: Capabilities) -> bool {
        match self {
            Self::Status | Self::Observe => matches!(
                capabilities,
                Capabilities::Observation | Capabilities::ObservationAndNavigation
            ),
            _ => matches!(
                capabilities,
                Capabilities::Navigation | Capabilities::ObservationAndNavigation
            ),
        }
    }
}

pub(super) fn catalog(capabilities: Capabilities) -> Value {
    let definitions = [
        (
            "handkite_status",
            "Observe the pinned iPhone and return lock state and capture metadata without screen content.",
        ),
        (
            "handkite_observe",
            "Capture one paired screenshot and accessibility tree from the pinned iPhone. Screen content is private.",
        ),
        (
            "handkite_acquire_control",
            "Explicitly acquire exclusive navigation control for this session for at most two minutes. Does not renew an existing lease.",
        ),
        (
            "handkite_release_control",
            "Release this session's navigation control.",
        ),
        (
            "handkite_home",
            "Press Home once using an expected observation revision. Returns delivery and observed effect separately, without screen content.",
        ),
        (
            "handkite_launch_clock",
            "Launch the fixed Clock app once using an expected observation revision. Returns delivery and observed effect separately, without screen content.",
        ),
    ];
    let tools: Vec<_> = definitions
        .into_iter()
        .filter_map(|(name, description)| {
            let tool = Tool::from_name(name)?;
            tool.allowed(capabilities).then(|| {
                let schema =
                    json!({"type": "object", "properties": {}, "additionalProperties": false});
                json!({"name": name, "description": description, "inputSchema": schema})
            })
        })
        .collect();
    json!({"tools": tools})
}
