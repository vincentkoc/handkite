use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use handkite_host::mcp::Capabilities;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::ssh_bridge;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};

fn key() -> String {
    let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
    wire.extend_from_slice(&[0x42; 32]);
    format!("ssh-ed25519 {}", STANDARD.encode(wire))
}

fn invoke(args: &[OsString], input: &[u8]) -> std::process::Output {
    let scratch = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_handkite-ssh-key"))
        .args(args)
        .env("HOME", scratch.path())
        .env(
            "SSH_ORIGINAL_COMMAND",
            "--capabilities all --socket /fixture/other",
        )
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    output
}

#[test]
fn entries_use_the_effective_account_and_explicit_capabilities_without_file_writes() {
    // Only account lookup and pure rendering: no real service or SSH files are read.
    let layout = ServiceLayout::current_account().unwrap();
    let key = key();
    for (value, capabilities) in [
        ("observation", Capabilities::Observation),
        ("navigation", Capabilities::Navigation),
        (
            "observation,navigation",
            Capabilities::ObservationAndNavigation,
        ),
    ] {
        for input in [key.clone(), format!("{key}\n")] {
            let output = invoke(&["--capabilities".into(), value.into()], input.as_bytes());
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert_eq!(
                output.stdout,
                ssh_bridge::render(&layout, &key, capabilities)
                    .unwrap()
                    .as_bytes()
            );
        }
    }
}

#[test]
fn invalid_arguments_never_echo_input_or_argument_values() {
    for args in [
        vec![],
        vec!["--help".into()],
        vec!["--capabilities".into(), "private-invalid-capability".into()],
        vec!["--capabilities".into(), OsString::from_vec(vec![0xff])],
        vec![
            "--capabilities".into(),
            "observation".into(),
            "--socket".into(),
        ],
        vec!["--key".into(), "private-key-argument".into()],
    ] {
        // Empty stdin also avoids a write/read race with immediate argument rejection.
        let output = invoke(&args, b"");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr == b"handkite-ssh-key: invalid capabilities\n"
                || output.stderr == b"handkite-ssh-key: usage: handkite-ssh-key --capabilities observation|navigation|observation,navigation < bare-public-key\n"
        );
    }
}

#[test]
fn invalid_public_keys_emit_only_the_fixed_error() {
    let key = key();
    for input in [
        Vec::new(),
        b"private-invalid-key".to_vec(),
        format!("{key} comment").into_bytes(),
        format!("restrict {key}").into_bytes(),
        format!("{key}\r\n").into_bytes(),
        format!("{key}\n{key}").into_bytes(),
        vec![b'x'; 80],
        vec![0xff; 80],
        vec![b'x'; 82],
    ] {
        let output = invoke(&["--capabilities".into(), "observation".into()], &input);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"handkite-ssh-key: invalid SSH public key\n");
    }
}
