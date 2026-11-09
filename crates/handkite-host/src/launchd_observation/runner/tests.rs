use super::*;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;

#[test]
fn builds_only_the_fixed_user_domain_query_without_environment_additions() {
    for uid in [0, 42, u32::MAX] {
        assert_eq!(
            observe_with(uid, |command, timeout| {
                assert_eq!(command.get_program(), "/bin/launchctl");
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    ["print", &format!("user/{uid}/com.handkite.host")]
                );
                assert_eq!(command.get_envs().count(), 0);
                assert_eq!(timeout, Duration::from_secs(5));
                Ok(ExitStatus::from_raw(0))
            }),
            Observation::Loaded
        );
    }
}

#[test]
fn only_zero_and_113_have_defined_advisory_results() {
    for code in 0..=255 {
        let expected = match code {
            0 => Observation::Loaded,
            113 => Observation::Absent,
            _ => Observation::Unknown,
        };
        assert_eq!(
            observe_with(42, |_, _| Ok(ExitStatus::from_raw(code << 8))),
            expected
        );
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        assert_eq!(
            observe_with(42, |_, _| Ok(ExitStatus::from_raw(signal))),
            Observation::Unknown
        );
    }
    for error in [RunError::Spawn, RunError::Wait, RunError::Timeout] {
        assert_eq!(observe_with(42, |_, _| Err(error)), Observation::Unknown);
    }
}

struct FakeChild {
    result: Option<io::Result<Option<ExitStatus>>>,
    events: Vec<&'static str>,
    cleanup_fails: bool,
}

impl ChildProcess for FakeChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.events.push("try_wait");
        self.result.take().unwrap_or(Ok(None))
    }
    fn kill(&mut self) -> io::Result<()> {
        self.events.push("kill");
        if self.cleanup_fails {
            Err(io::Error::other("synthetic failure"))
        } else {
            Ok(())
        }
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.events.push("wait");
        if self.cleanup_fails {
            Err(io::Error::other("synthetic failure"))
        } else {
            Ok(ExitStatus::from_raw(libc::SIGKILL))
        }
    }
}

#[test]
fn wait_failure_and_timeout_attempt_cleanup_without_promoting_the_result() {
    for cleanup_fails in [false, true] {
        let mut child = FakeChild {
            result: Some(Err(io::Error::other("synthetic wait failure"))),
            events: vec![],
            cleanup_fails,
        };
        assert_eq!(wait_child(&mut child, TIMEOUT), Err(RunError::Wait));
        assert_eq!(child.events, ["try_wait", "kill", "wait"]);
        child.events.clear();
        assert_eq!(
            wait_child(&mut child, Duration::ZERO),
            Err(RunError::Timeout)
        );
        assert_eq!(child.events, ["kill", "wait"]);
    }
}

#[test]
fn an_exited_child_is_not_killed_again() {
    let mut child = FakeChild {
        result: Some(Ok(Some(ExitStatus::from_raw(113 << 8)))),
        events: vec![],
        cleanup_fails: false,
    };
    assert_eq!(
        wait_child(&mut child, TIMEOUT),
        Ok(ExitStatus::from_raw(113 << 8))
    );
    assert_eq!(child.events, ["try_wait"]);
}

#[test]
fn spawn_failure_is_unknown() {
    assert_eq!(
        observe_with(42, |_, timeout| {
            let mut command = Command::new("/no-such-handkite-launchd-fixture");
            protect_child(&mut command);
            run_command(&mut command, timeout)
        }),
        Observation::Unknown
    );
}

fn fixture_command(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--ignored", "--exact", name, "--nocapture"]);
    command.env("HANDKITE_TEST_ENV", "fixture");
    protect_child(&mut command);
    command
}

#[test]
fn child_environment_and_all_stdio_are_cleared() {
    let mut command =
        fixture_command("launchd_observation::runner::tests::stdio_environment_child");
    // The fixture exits with a distinctive code only after its assertions run.
    assert_eq!(run_command(&mut command, TIMEOUT).unwrap().code(), Some(73));
}

#[test]
#[ignore = "test-owned subprocess fixture only"]
fn stdio_environment_child() {
    assert!(std::env::var_os("HANDKITE_TEST_ENV").is_none());
    assert!(std::env::var_os("HOME").is_none());
    assert!(std::env::var_os("PATH").is_none());
    let null_device = std::fs::metadata("/dev/null").unwrap().rdev();
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(unsafe { libc::fstat(fd, metadata.as_mut_ptr()) }, 0);
        let metadata = unsafe { metadata.assume_init() };
        assert_eq!(metadata.st_mode & libc::S_IFMT, libc::S_IFCHR);
        assert_eq!(metadata.st_rdev as u64, null_device);
    }
    std::process::exit(73);
}

#[test]
fn timeout_kills_and_reaps_only_its_noisy_test_child() {
    let mut child = fixture_command("launchd_observation::runner::tests::noisy_child")
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    let start = Instant::now();
    assert_eq!(
        wait_child(&mut child, Duration::from_millis(100)),
        Err(RunError::Timeout)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(child.try_wait().unwrap().is_some());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
#[ignore = "test-owned subprocess fixture only"]
fn noisy_child() {
    let output = "synthetic diagnostic".repeat(1024);
    loop {
        println!("{output}");
        eprintln!("{output}");
    }
}
