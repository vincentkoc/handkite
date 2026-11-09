use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

const MAX_CONFIG_BYTES: u64 = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    device_key: DeviceKey,
    usb_id: String,
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
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("configuration exceeds 4 KiB");
    }
    let config: ConfigFile =
        serde_json::from_slice(&bytes).map_err(|_| "configuration is invalid")?;
    let usb_id = HardwareId::new(config.usb_id).map_err(|_| "device identity is invalid")?;
    Ok(DeviceBinding {
        key: config.device_key,
        usb_id,
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
}
