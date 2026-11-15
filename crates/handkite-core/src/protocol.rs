use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DeviceKey(String);

impl DeviceKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ContractError::InvalidDeviceKey);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DeviceKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HardwareId(String);

impl HardwareId {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 || value.chars().any(char::is_whitespace) {
            return Err(ContractError::InvalidHardwareId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HardwareId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HardwareId(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub struct DeviceBinding {
    pub key: DeviceKey,
    pub usb_id: HardwareId,
}

impl DeviceBinding {
    pub fn verify(
        &self,
        requested: &DeviceKey,
        connected_usb: &HardwareId,
        wda_target: &HardwareId,
    ) -> Result<(), ContractError> {
        if requested != &self.key || connected_usb != &self.usb_id || wda_target != &self.usb_id {
            return Err(ContractError::DeviceMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    Locked,
    Unlocked,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(pub u64);

impl Revision {
    pub fn next(self) -> Result<Self, ContractError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(ContractError::RevisionExhausted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonTarget {
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ButtonTarget {
    pub fn valid(&self) -> bool {
        !self.label.trim().is_empty()
            && self.label.len() <= 512
            && !self.label.chars().any(char::is_control)
            && [self.x, self.y, self.width, self.height]
                .into_iter()
                .all(f64::is_finite)
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 10_000.0
            && self.y + self.height <= 10_000.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub lock: LockState,
    pub wda_ready: bool,
    pub foreground_app: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservationWindow {
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
}

impl ObservationWindow {
    pub fn duration_ms(&self) -> Result<u64, ContractError> {
        self.finished_unix_ms
            .checked_sub(self.started_unix_ms)
            .ok_or(ContractError::InvalidObservationWindow)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    pub device: DeviceKey,
    pub revision: Revision,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub accessibility_tree: String,
    pub screenshot: ScreenImage,
}

/// A captured image, without an inferred device state or actionable revision.
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageObservation {
    pub device: DeviceKey,
    pub window: ObservationWindow,
    pub screenshot: ScreenImage,
}

#[derive(Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    #[default]
    Paired,
    ImageOnly,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScreenImage {
    pub format: ImageFormat,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Png,
    Jpeg,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    ClickButton {
        target: ButtonTarget,
    },
    Tap {
        at: Point,
    },
    DoubleTap {
        at: Point,
    },
    LongPress {
        at: Point,
        duration_ms: u32,
    },
    Swipe {
        from: Point,
        to: Point,
        duration_ms: u32,
    },
    Text {
        value: String,
    },
    /// Types into the current editing selection, without preserving its old range.
    TextCurrent {
        value: String,
    },
    Key {
        key: KeyboardKey,
    },
    /// Applies to the current editing selection, without preserving its old range.
    KeyCurrent {
        key: KeyboardKey,
    },
    Home,
    LaunchApp {
        bundle_id: String,
    },
    Lock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardKey {
    Backspace,
    Enter,
    Tab,
    Escape,
}

/// Largest text accepted for one direct injection, in UTF-8 bytes.
pub const TEXT_MAX_BYTES: usize = 4_096;

/// Whether `value` may be injected as literal text in one request.
///
/// The value is sent whole and never normalized, truncated, or split, so every
/// grapheme cluster (ZWJ emoji, skin tones, flags, variation selectors,
/// combining marks) reaches the device as the caller wrote it. Refused:
/// - empty or oversized values; the limit is bytes, checked before any send;
/// - control characters, including newline and tab: XCTest turns them into
///   key presses that can submit a form. Use the key action for those;
/// - U+2028 and U+2029, which editors treat as line breaks;
/// - U+F700..=U+F8FF, the AppKit function-key range. XCTest text synthesis
///   reads these as arrows, forward delete, and other hardware keys, so they
///   would move the caret or delete content instead of inserting text.
pub fn injectable_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= TEXT_MAX_BYTES
        && !value.chars().any(|scalar| {
            scalar.is_control()
                || matches!(scalar, '\u{2028}' | '\u{2029}' | '\u{f700}'..='\u{f8ff}')
        })
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ClickButton { .. } => "click_button",
            Self::Tap { .. } => "tap",
            Self::DoubleTap { .. } => "double_tap",
            Self::LongPress { .. } => "long_press",
            Self::Swipe { .. } => "swipe",
            Self::Text { .. } | Self::TextCurrent { .. } => "text",
            Self::Key { .. } | Self::KeyCurrent { .. } => "key",
            Self::Home => "home",
            Self::LaunchApp { .. } => "launch_app",
            Self::Lock => "lock",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub device: DeviceKey,
    pub expected_revision: Option<Revision>,
    pub action: Action,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransportReceipt {
    /// None means the transport outcome is unknown, not that delivery failed.
    pub accepted: Option<bool>,
    pub action_kind: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub receipt: TransportReceipt,
    pub after: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_image: Option<ImageObservation>,
    /// Present only for lock actions; Uncertain means native proof was unavailable.
    pub native_lock: Option<LockState>,
    pub effect: ObservedEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedEffect {
    Changed,
    /// Native state is locked after the request; this does not prove a transition.
    LockConfirmed,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractError {
    InvalidDeviceKey,
    InvalidHardwareId,
    DeviceMismatch,
    InvalidObservationWindow,
    RevisionExhausted,
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDeviceKey => "invalid device key",
            Self::InvalidHardwareId => "invalid hardware identity",
            Self::DeviceMismatch => "requested, connected, and WDA device identities differ",
            Self::InvalidObservationWindow => "observation window ends before it starts",
            Self::RevisionExhausted => "observation revision exhausted",
        })
    }
}

impl std::error::Error for ContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_rejects_a_second_phone_and_wrong_wda_target() {
        let key = DeviceKey::new("phone").unwrap();
        let expected = HardwareId::new("usb-a").unwrap();
        let other = HardwareId::new("usb-b").unwrap();
        let binding = DeviceBinding {
            key: key.clone(),
            usb_id: expected.clone(),
        };

        assert!(binding.verify(&key, &expected, &expected).is_ok());
        assert_eq!(
            binding.verify(&key, &other, &expected),
            Err(ContractError::DeviceMismatch)
        );
        assert_eq!(
            binding.verify(&key, &expected, &other),
            Err(ContractError::DeviceMismatch)
        );
        assert_eq!(
            binding.verify(&DeviceKey::new("other").unwrap(), &expected, &expected),
            Err(ContractError::DeviceMismatch)
        );
        assert!(!format!("{binding:?}").contains("usb-a"));
    }

    #[test]
    fn invalid_window_cannot_look_fresh() {
        let window = ObservationWindow {
            started_unix_ms: 9,
            finished_unix_ms: 8,
        };
        assert_eq!(
            window.duration_ms(),
            Err(ContractError::InvalidObservationWindow)
        );
    }

    #[test]
    fn decoded_device_key_still_obeys_identity_rules() {
        assert_eq!(
            serde_json::from_str::<DeviceKey>("\"phone_1\"").unwrap(),
            DeviceKey::new("phone_1").unwrap()
        );
        for invalid in ["\"\"", "\"other phone\"", "\"../phone\""] {
            assert!(serde_json::from_str::<DeviceKey>(invalid).is_err());
        }
    }

    #[test]
    fn injectable_text_keeps_clusters_and_refuses_keys_and_line_breaks() {
        for accepted in [
            "plain ASCII",
            "\u{4f60}\u{597d}",
            "\u{304b}\u{3099}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
            "\u{1f44b}\u{1f3fd}",
            "\u{1f1ef}\u{1f1f5}",
            "\u{2764}\u{fe0f}",
            "\u{645}\u{6cc}\u{200c}\u{62e}",
            "\u{e000}\u{f6ff}\u{f900}",
        ] {
            assert!(injectable_text(accepted));
        }
        let limit = "a".repeat(TEXT_MAX_BYTES);
        assert!(injectable_text(&limit));
        // The limit is bytes: the last scalar must fit whole or the text is refused.
        let wide = "\u{1f600}".repeat(TEXT_MAX_BYTES / 4);
        assert!(injectable_text(&wide));
        for refused in [
            String::new(),
            limit + "a",
            wide + "a",
            "\n".into(),
            "\r".into(),
            "\t".into(),
            "\0".into(),
            "\u{7f}".into(),
            "\u{85}".into(),
            "\u{2028}".into(),
            "\u{2029}".into(),
            "\u{f700}".into(),
            "\u{f728}".into(),
            "\u{f8ff}".into(),
        ] {
            assert!(!injectable_text(&refused));
        }
    }
}
