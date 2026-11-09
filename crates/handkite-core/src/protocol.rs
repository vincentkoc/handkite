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
    Tap {
        at: Point,
    },
    Swipe {
        from: Point,
        to: Point,
        duration_ms: u32,
    },
    Text {
        value: String,
    },
    Home,
    LaunchApp {
        bundle_id: String,
    },
    Lock,
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Tap { .. } => "tap",
            Self::Swipe { .. } => "swipe",
            Self::Text { .. } => "text",
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
    pub accepted: bool,
    pub action_kind: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub receipt: TransportReceipt,
    pub after: Option<Observation>,
    pub effect: ObservedEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedEffect {
    Changed,
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
}
