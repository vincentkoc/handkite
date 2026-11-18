use serde_json::{Value, json};

/// Selected by the host launch command, never by MCP request arguments.
#[derive(Clone, Copy, Default)]
pub enum Capabilities {
    #[default]
    Observation,
    Navigation,
    ObservationAndNavigation,
    ObservationNavigationInteraction,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Tool {
    Status,
    Observe,
    Acquire,
    Release,
    Home,
    Clock,
    Tap,
    TapElement,
    TextElement,
    ClickButton,
    DoubleTap,
    LongPress,
    Swipe,
    Text,
    Key,
    LaunchApp,
}

impl Tool {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "handkite_status" => Some(Self::Status),
            "handkite_observe" => Some(Self::Observe),
            "handkite_acquire_control" => Some(Self::Acquire),
            "handkite_release_control" => Some(Self::Release),
            "handkite_home" => Some(Self::Home),
            "handkite_launch_clock" => Some(Self::Clock),
            "handkite_tap_element" => Some(Self::TapElement),
            "handkite_text_element" => Some(Self::TextElement),
            "handkite_tap" => Some(Self::Tap),
            "handkite_click_button" => Some(Self::ClickButton),
            "handkite_double_tap" => Some(Self::DoubleTap),
            "handkite_long_press" => Some(Self::LongPress),
            "handkite_swipe" => Some(Self::Swipe),
            "handkite_text" => Some(Self::Text),
            "handkite_key" => Some(Self::Key),
            "handkite_launch_app" => Some(Self::LaunchApp),
            _ => None,
        }
    }

    pub(super) fn allowed(self, capabilities: Capabilities) -> bool {
        match self {
            Self::TapElement
            | Self::TextElement
            | Self::Tap
            | Self::ClickButton
            | Self::DoubleTap
            | Self::LongPress
            | Self::Swipe
            | Self::Text
            | Self::Key
            | Self::LaunchApp => {
                matches!(capabilities, Capabilities::ObservationNavigationInteraction)
            }
            Self::Status | Self::Observe => matches!(
                capabilities,
                Capabilities::Observation
                    | Capabilities::ObservationAndNavigation
                    | Capabilities::ObservationNavigationInteraction
            ),
            _ => matches!(
                capabilities,
                Capabilities::Navigation
                    | Capabilities::ObservationAndNavigation
                    | Capabilities::ObservationNavigationInteraction
            ),
        }
    }

    pub(super) fn is_action(self) -> bool {
        matches!(
            self,
            Self::Home
                | Self::Clock
                | Self::Tap
                | Self::ClickButton
                | Self::DoubleTap
                | Self::LongPress
                | Self::Swipe
                | Self::Text
                | Self::Key
                | Self::LaunchApp
        )
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
            "Explicitly acquire exclusive navigation control. Verified successful actions renew the two-minute lease; observations, refusals and uncertain input do not. Release explicitly when finished.",
        ),
        (
            "handkite_release_control",
            "Release this session's navigation control.",
        ),
        (
            "handkite_home",
            "Press Home once using an expected observation revision. Returns delivery, observed effect, and the captured screen when observation is authorized.",
        ),
        (
            "handkite_launch_clock",
            "Launch the fixed Clock app once using an expected observation revision. Returns delivery, observed effect, and the captured screen when observation is authorized.",
        ),
        (
            "handkite_tap_element",
            "Click one opaque native target once. Owned native taps return an image and pending effect; read the correlated effect_updates on the next observation. Pending never permits replay. No coordinate fallback.",
        ),
        (
            "handkite_text_element",
            "Type once into an already focused opaque native target; never changes focus. Exact readback required.",
        ),
        (
            "handkite_tap",
            "Tap one screen point using an expected observation revision and return the captured screen.",
        ),
        (
            "handkite_click_button",
            "Click one uniquely labelled Button or MenuItem by its observed label and rectangle. Uses element identity, not a coordinate fallback. Requires an expected observation revision.",
        ),
        (
            "handkite_double_tap",
            "Double-tap one screen point as a single gesture using an expected observation revision.",
        ),
        (
            "handkite_long_press",
            "Press and hold one screen point for a bounded duration using an expected observation revision.",
        ),
        (
            "handkite_swipe",
            "Swipe once between two screen points using an expected observation revision and return the captured screen.",
        ),
        (
            "handkite_text",
            "Type text once using an expected observation revision and return the captured screen. Control characters are rejected.",
        ),
        (
            "handkite_key",
            "Press one editing key: backspace, enter, tab or escape. Enter can submit the current form. Requires an expected observation revision.",
        ),
        (
            "handkite_launch_app",
            "Launch one app by bundle identifier using an expected observation revision and return the captured screen.",
        ),
    ];
    let tools: Vec<_> = definitions
        .into_iter()
        .filter_map(|(name, description)| {
            let tool = Tool::from_name(name)?;
            tool.allowed(capabilities).then(|| {
                let schema = if matches!(tool, Tool::TapElement | Tool::TextElement) {
                    super::target_tools::schema(tool == Tool::TextElement)
                } else if tool.is_action() {
                    super::interaction::schema(tool)
                } else if tool == Tool::Observe {
                    super::presentation::schema()
                } else {
                    json!({"type": "object", "properties": {}, "additionalProperties": false})
                };
                json!({"name": name, "description": description, "inputSchema": schema})
            })
        })
        .collect();
    json!({"tools": tools})
}
