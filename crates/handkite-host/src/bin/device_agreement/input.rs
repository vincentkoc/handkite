use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};
use handkite_host::release_verify;
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;

const MAX_INPUT_BYTES: u64 = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedInput {
    schema_version: u32,
    device_key: DeviceKey,
    usb_id: String,
}

pub(super) fn read_binding(input: &mut File) -> Option<DeviceBinding> {
    let metadata = input.metadata().ok()?;
    // Descriptor hygiene does not establish independent provenance of the pin.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > MAX_INPUT_BYTES
    {
        return None;
    }
    release_verify::check_private_input_acl(input).ok()?;
    if input.stream_position().ok()? != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 != metadata.len() {
        return None;
    }
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Option<DeviceBinding> {
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return None;
    }
    let input: PinnedInput = serde_json::from_slice(bytes).ok()?;
    if input.schema_version != 1 || input.usb_id.chars().any(char::is_control) {
        return None;
    }
    Some(DeviceBinding {
        key: input.device_key,
        usb_id: HardwareId::new(input.usb_id).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{SeekFrom, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::{fs::PermissionsExt, net::UnixStream};
    use tempfile::NamedTempFile;

    const VALID: &[u8] = br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#;

    fn input(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[test]
    fn exact_versioned_schema_preserves_both_binding_fields() {
        let binding = parse(VALID).unwrap();
        assert_eq!(binding.key.as_str(), "phone");
        assert_eq!(binding.usb_id.as_str(), "test-device");
        let binding = parse(
            br#" { "usb_id": "TEST-Device", "device_key": "Phone_2", "schema_version": 1 }
"#,
        )
        .unwrap();
        assert_eq!(binding.key.as_str(), "Phone_2");
        assert_eq!(binding.usb_id.as_str(), "TEST-Device");
    }

    #[test]
    fn rejects_unknown_duplicate_missing_and_mistyped_fields() {
        for bytes in [
            b"".as_slice(),
            b"[]",
            b"null",
            b"{}",
            br#"{"device_key":"phone","usb_id":"test-device"}"#,
            br#"{"schema_version":1,"usb_id":"test-device"}"#,
            br#"{"schema_version":1,"device_key":"phone"}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","fallback":false}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","wda_runner_bundle_id":"com.example.xctrunner"}"#,
            br#"{"schema_version":1,"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#,
            br#"{"schema_version":1,"device_key":"phone","device_key":"phone","usb_id":"test-device"}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","usb_id":"test-device"}"#,
            br#"{"schema_version":1,"device_key":null,"usb_id":"test-device"}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":123}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":null}"#,
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}{}"#,
            b"\xff",
        ] {
            assert!(parse(bytes).is_none());
        }
        for version in ["0", "2", "-1", "1.0", "true", "null", "\"1\"", "4294967296"] {
            let value = format!(
                "{{\"schema_version\":{version},\"device_key\":\"phone\",\"usb_id\":\"test-device\"}}"
            );
            assert!(parse(value.as_bytes()).is_none());
        }
    }

    #[test]
    fn enforces_binding_and_envelope_bounds_without_normalization() {
        for (key, usb) in [
            ("".to_owned(), "test-device".to_owned()),
            ("a".repeat(65), "test-device".to_owned()),
            ("../phone".to_owned(), "test-device".to_owned()),
            (" phone".to_owned(), "test-device".to_owned()),
            ("phone".to_owned(), String::new()),
            ("phone".to_owned(), "x".repeat(129)),
            ("phone".to_owned(), "test-device ".to_owned()),
            ("phone".to_owned(), "test\ndevice".to_owned()),
            ("phone".to_owned(), "test\0device".to_owned()),
            ("phone".to_owned(), "test\u{7f}device".to_owned()),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1, "device_key": key, "usb_id": usb,
            }))
            .unwrap();
            assert!(parse(&bytes).is_none());
        }
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1, "device_key": "a".repeat(64), "usb_id": "x".repeat(128),
        }))
        .unwrap();
        assert!(parse(&bytes).is_some());
        let mut bytes = VALID.to_vec();
        bytes.resize(MAX_INPUT_BYTES as usize, b' ');
        assert!(parse(&bytes).is_some());
        bytes.push(b' ');
        assert!(parse(&bytes).is_none());
    }

    #[test]
    fn rejects_unsafe_modes_and_sizes_before_reading() {
        for mode in [0o400, 0o640, 0o666, 0o700, 0o4600] {
            let mut file = input(VALID);
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(read_binding(file.as_file_mut()).is_none());
            assert_eq!(file.stream_position().unwrap(), 0);
        }
        for bytes in [vec![], vec![b' '; MAX_INPUT_BYTES as usize + 1]] {
            let mut file = input(&bytes);
            assert!(read_binding(file.as_file_mut()).is_none());
            assert_eq!(file.stream_position().unwrap(), 0);
        }
    }

    #[test]
    fn rejects_devices_directories_sockets_and_multiple_links_without_reading() {
        let mut device = File::open("/dev/null").unwrap();
        assert!(read_binding(&mut device).is_none());
        let directory = tempfile::tempdir().unwrap();
        assert!(read_binding(&mut File::open(directory.path()).unwrap()).is_none());
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert!(read_binding(&mut File::from(OwnedFd::from(socket))).is_none());
        let mut file = input(VALID);
        std::fs::hard_link(file.path(), directory.path().join("linked")).unwrap();
        assert!(read_binding(file.as_file_mut()).is_none());
        assert_eq!(file.stream_position().unwrap(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn admits_private_regular_file_and_requires_initial_offset() {
        let mut file = input(VALID);
        let binding = read_binding(file.as_file_mut()).unwrap();
        assert_eq!(binding.key.as_str(), "phone");
        assert_eq!(binding.usb_id.as_str(), "test-device");
        let mut file = input(&[b"prefix".as_slice(), VALID].concat());
        file.seek(SeekFrom::Start(6)).unwrap();
        assert!(read_binding(file.as_file_mut()).is_none());
        assert_eq!(file.stream_position().unwrap(), 6);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_nonempty_acl_before_reading() {
        let mut file = input(VALID);
        assert!(
            std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg("everyone deny write")
                .arg(file.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(read_binding(file.as_file_mut()).is_none());
        assert_eq!(file.stream_position().unwrap(), 0);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_rejects_before_reading() {
        let mut file = input(VALID);
        assert!(read_binding(file.as_file_mut()).is_none());
        assert_eq!(file.stream_position().unwrap(), 0);
    }
}
