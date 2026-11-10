use crate::wda_bundle::validate_runner_bundle_id;
use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};
use serde::{Deserialize, Deserializer};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(super) const MAX_CONFIG_BYTES: u64 = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    device_key: DeviceKey,
    usb_id: String,
    #[serde(default, deserialize_with = "deserialize_runner_bundle_id")]
    wda_runner_bundle_id: Option<String>,
}

fn deserialize_runner_bundle_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let bundle_id = String::deserialize(deserializer)?;
    validate_runner_bundle_id(&bundle_id).map_err(serde::de::Error::custom)?;
    Ok(Some(bundle_id))
}

/// Parsed configuration. Device identity and runner ID belong to the private host.
/// This value intentionally has no logging or serialization implementation.
pub struct HostConfig {
    binding: DeviceBinding,
    wda_runner_bundle_id: Option<String>,
}

impl HostConfig {
    pub fn binding(&self) -> &DeviceBinding {
        &self.binding
    }

    /// The exact configured runner ID, if present; not proof it is installed.
    pub fn wda_runner_bundle_id(&self) -> Option<&str> {
        self.wda_runner_bundle_id.as_deref()
    }

    pub(super) fn into_binding(self) -> DeviceBinding {
        self.binding
    }
}

pub fn load(path: &Path) -> Result<DeviceBinding, &'static str> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "configuration unavailable")?;
    let metadata = file.metadata().map_err(|_| "configuration unavailable")?;
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_CONFIG_BYTES
    {
        return Err("configuration must be a regular mode-0600 file under 4 KiB");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "configuration unreadable")?;
    parse(&bytes).map(HostConfig::into_binding)
}

pub(super) fn parse(bytes: &[u8]) -> Result<HostConfig, &'static str> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("configuration exceeds 4 KiB");
    }
    let config: ConfigFile =
        serde_json::from_slice(bytes).map_err(|_| "configuration is invalid")?;
    let usb_id = HardwareId::new(config.usb_id).map_err(|_| "device identity is invalid")?;
    Ok(HostConfig {
        binding: DeviceBinding {
            key: config.device_key,
            usb_id,
        },
        wda_runner_bundle_id: config.wda_runner_bundle_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fields_are_bounded_and_pinned() {
        let valid: ConfigFile =
            serde_json::from_str(r#"{"device_key":"iphone","usb_id":"test-device"}"#).unwrap();
        assert_eq!(valid.device_key.as_str(), "iphone");
        assert!(
            serde_json::from_str::<ConfigFile>(
                r#"{"device_key":"../iphone","usb_id":"test-device"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ConfigFile>(
                r#"{"device_key":"iphone","usb_id":"test-device","fallback":"ipad"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn runner_id_is_optional_and_retains_the_exact_validated_value() {
        let absent = parse(br#"{"device_key":"phone","usb_id":"test-device"}"#).unwrap();
        assert_eq!(absent.binding().key.as_str(), "phone");
        assert_eq!(absent.wda_runner_bundle_id(), None);
        for bundle_id in [
            "com.example.WDA-Runner.xctrunner".to_owned(),
            format!("com.{}.xctrunner", "a".repeat(241)),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "device_key": "phone",
                "usb_id": "test-device",
                "wda_runner_bundle_id": bundle_id,
            }))
            .unwrap();
            let config = parse(&bytes).unwrap();
            assert_eq!(config.wda_runner_bundle_id(), Some(bundle_id.as_str()));
            assert_eq!(config.binding().usb_id.as_str(), "test-device");
        }
    }

    #[test]
    fn present_runner_id_requires_one_strict_string_with_fixed_errors() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("com.example.Runner"),
            serde_json::json!("example.xctrunner"),
            serde_json::json!("com..xctrunner"),
            serde_json::json!("com.example.*.xctrunner"),
            serde_json::json!("com.example/xctrunner"),
            serde_json::json!("com.example.xctrunner "),
            serde_json::json!("com.example.XCTRunner"),
            serde_json::json!("com.ex\u{00e4}mple.xctrunner"),
            serde_json::json!(format!("com.{}.xctrunner", "a".repeat(242))),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "device_key": "phone",
                "usb_id": "test-device",
                "wda_runner_bundle_id": value,
            }))
            .unwrap();
            assert_eq!(parse(&bytes).err(), Some("configuration is invalid"));
        }
        assert_eq!(
            parse(br#"{"device_key":"phone","usb_id":"test-device","wda_runner_bundle_id":"com.example.xctrunner","wda_runner_bundle_id":"com.example.xctrunner"}"#).err(),
            Some("configuration is invalid")
        );
    }
}
