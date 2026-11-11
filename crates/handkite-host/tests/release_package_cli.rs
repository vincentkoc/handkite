use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_handkite-release-package"))
}

#[test]
fn invalid_arguments_print_only_fixed_status() {
    let output = command().arg("--unexpected").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"release_package=invalid\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn open_pipe_rejects_without_waiting_for_eof_or_entering_signing() {
    let mut child = command()
        .args(["--source", "/private/source", "--output", "/private/output"])
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
    assert_eq!(output.stdout, b"release_package=invalid\n");
    assert!(output.stderr.is_empty());
}
