use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn invalid_arguments_have_only_fixed_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_handkite-controller-process"))
        .arg("--unexpected")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, b"controller_process=unknown\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn open_pipe_refuses_before_waiting_for_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_handkite-controller-process"))
        .arg("--pinned-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let writer = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited_without_eof = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(writer);
    let output = child.wait_with_output().unwrap();
    assert!(exited_without_eof, "input waited for pipe EOF");
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, b"controller_process=unknown\n");
    assert!(output.stderr.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn protected_pins_compare_only_the_current_test_process_without_disclosure() {
    use std::io::{Seek, SeekFrom, Write};
    use std::mem::{MaybeUninit, size_of};

    let pid = std::process::id() as i32;
    let mut bsd = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let mut usage = MaybeUninit::<libc::rusage_info_v0>::zeroed();
    unsafe {
        assert_eq!(
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                bsd.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>() as i32
            ),
            size_of::<libc::proc_bsdinfo>() as i32
        );
        assert_eq!(
            libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()),
            0
        );
    }
    let bsd = unsafe { bsd.assume_init() };
    let usage = unsafe { usage.assume_init() };
    let uuid: String = usage
        .ri_uuid
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut pins = serde_json::json!({
        "schema_version": 1, "pid": pid,
        "start_seconds": bsd.pbi_start_tvsec,
        "start_microseconds": bsd.pbi_start_tvusec,
        "start_abstime": usage.ri_proc_start_abstime,
        "effective_uid": bsd.pbi_uid,
        "executable_path": std::env::current_exe().unwrap(),
        "executable_uuid": uuid,
    });
    for (code, expected) in [
        (0, b"controller_process=match\n".as_slice()),
        (1, b"controller_process=mismatch\n"),
    ] {
        let mut input = tempfile::NamedTempFile::new().unwrap();
        input
            .write_all(&serde_json::to_vec(&pins).unwrap())
            .unwrap();
        input.seek(SeekFrom::Start(0)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-controller-process"))
            .arg("--pinned-stdin")
            .stdin(input.as_file().try_clone().unwrap())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
        pins["start_abstime"] = (usage.ri_proc_start_abstime + 1).into();
    }
}
