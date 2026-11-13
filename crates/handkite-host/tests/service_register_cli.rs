use std::process::{Command, Stdio};

#[test]
fn invalid_arguments_and_missing_binding_refuse_before_setup() {
    for args in [
        vec!["--unexpected"],
        vec!["--binding-fd", "3"],
        vec!["--binding-fd", "3", "--bootstrap"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-service-register"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"service_register=invalid\n");
        assert!(output.stderr.is_empty());
    }
}
