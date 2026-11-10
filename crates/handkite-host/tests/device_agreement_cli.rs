use std::ffi::OsString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn invalid_production_invocations_have_only_fixed_output() {
    // Never invoke the valid form here: it reaches the account's real config.
    for args in [
        vec![],
        vec![OsString::from("--help")],
        vec!["--service=true".into()],
        vec!["--pinned-stdin=true".into()],
        vec!["--pinned-stdin".into(), "--service".into()],
        vec!["--service".into(), "--pinned-stdin".into()],
        vec!["--pinned-stdin".into(), "--pinned-stdin".into()],
        vec![
            "--pinned-stdin".into(),
            "--config".into(),
            "/fixture".into(),
        ],
        vec!["--service".into(), "--service".into()],
        vec!["--service".into(), "--config".into(), "/fixture".into()],
        vec![OsString::from_vec(b"--service\xff".to_vec())],
        vec!["--service".into(), OsString::from_vec(vec![0xff])],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-device-agreement"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stdout, b"device_agreement=reject stage=arguments\n");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn rejected_pinned_input_never_reaches_live_service_setup() {
    const VALID: &[u8] = br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#;
    // Only unsafe descriptors or invalid payloads reach the real binary here.
    let mut inputs = vec![Stdio::null(), Stdio::piped()];
    let mut files = Vec::new();
    for (bytes, mode) in [
        (VALID, 0o644),
        (b"".as_slice(), 0o600),
        (b"private-invalid-payload".as_slice(), 0o600),
        (
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","extra":true}"#
                .as_slice(),
            0o600,
        ),
    ] {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
        inputs.push(Stdio::from(File::open(file.path()).unwrap()));
        files.push(file);
    }
    for input in inputs {
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-device-agreement"))
            .arg("--pinned-stdin")
            .stdin(input)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            output.stdout,
            b"device_agreement=reject stage=pinned_input\n"
        );
        assert!(output.stderr.is_empty());
    }
}
