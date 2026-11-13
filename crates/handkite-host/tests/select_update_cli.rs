use std::process::{Command, Stdio};

#[test]
fn invalid_arguments_or_missing_input_descriptors_have_fixed_output() {
    for args in [
        vec!["--unexpected"],
        vec![
            "--operation",
            "upgrade",
            "--binding-fd",
            "3",
            "--previous-fd",
            "4",
        ],
        vec![
            "--operation",
            "rollback",
            "--binding-fd",
            "3",
            "--previous-fd",
            "4",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-select-update"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"update_select=invalid\n");
        assert!(output.stderr.is_empty());
    }
}
