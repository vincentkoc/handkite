use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;

#[test]
fn invalid_production_invocations_have_only_fixed_output() {
    // Never invoke the valid form here: it reaches the account's real config.
    for args in [
        vec![],
        vec![OsString::from("--help")],
        vec!["--service=true".into()],
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
        assert_eq!(output.stdout, b"device_agreement=reject\n");
        assert!(output.stderr.is_empty());
    }
}
