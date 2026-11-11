use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn invalid_arguments_have_only_fixed_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_handkite-service-stage"))
        .arg("--unexpected")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"service_stage=invalid\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_open_pipe_refuses_without_waiting_or_entering_setup() {
    let mut binding = tempfile::NamedTempFile::new().unwrap();
    binding
        .write_all(br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#)
        .unwrap();
    binding.seek(SeekFrom::Start(0)).unwrap();
    let binding_fd = binding.as_file().as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_handkite-service-stage"));
    command
        .args(["--binding-fd", "3"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(binding_fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let writer = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("invalid input waited for pipe EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    drop(writer);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"service_stage=invalid\n");
    assert!(output.stderr.is_empty());
}
