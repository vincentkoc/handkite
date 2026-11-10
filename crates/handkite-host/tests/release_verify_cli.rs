use std::io::{Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_handkite-release-verify"))
}

#[test]
fn invalid_arguments_have_only_fixed_output() {
    let output = command().arg("--unexpected").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"release_verify=invalid\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn refuses_an_open_pipe_without_waiting_for_eof() {
    let mut child = command()
        .args(["--stage", "/stage"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let writer = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("regular-file admission waited for pipe EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    drop(writer);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"release_verify=invalid\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn production_verifier_rejection_has_only_fixed_output() {
    let mut input = tempfile::NamedTempFile::new().unwrap();
    // Invalid manifest bytes reject before the production route accesses a stage.
    write!(input, "{}\n{}\nnot-json", "a".repeat(64), "b".repeat(40)).unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();
    let output = command()
        .args(["--stage", "/stage"])
        .stdin(input.reopen().unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"release_verify=reject\n");
    assert!(output.stderr.is_empty());
}
