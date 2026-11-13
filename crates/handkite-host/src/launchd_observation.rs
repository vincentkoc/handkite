//! Advisory observation of one fixed service in the effective account's user domain.

/// This is deliberately separate from installer activity evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// launchctl reported no such service in this user domain only.
    Absent,
    /// The service was found; this says nothing about a running process or readiness.
    Loaded,
    Unknown,
}

/// Query only `user/<effective uid>/com.handkite.host` without retaining tool output.
///
/// No result proves a controller stopped, excludes services in other domains, or
/// grants install/activation authority. Non-macOS returns Unknown without spawning.
pub fn observe() -> Observation {
    #[cfg(target_os = "macos")]
    {
        runner::observe()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Observation::Unknown
    }
}

/// Observe the exact job in both account domains; neither result is process proof.
pub(crate) fn installer_jobs() -> [Observation; 2] {
    #[cfg(target_os = "macos")]
    {
        runner::observe_pair_with(unsafe { libc::geteuid() }, runner::run_command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        [Observation::Unknown; 2]
    }
}

#[cfg(any(target_os = "macos", test))]
mod runner {
    use super::Observation;
    use std::io;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[cfg(target_os = "macos")]
    pub(super) fn observe() -> Observation {
        observe_with(unsafe { libc::geteuid() }, run_command)
    }

    fn observe_with(
        uid: u32,
        run: impl FnOnce(&mut Command, Duration) -> Result<ExitStatus, RunError>,
    ) -> Observation {
        observe_domain(uid, "user", run)
    }

    pub(super) fn observe_pair_with(
        uid: u32,
        mut run: impl FnMut(&mut Command, Duration) -> Result<ExitStatus, RunError>,
    ) -> [Observation; 2] {
        ["user", "gui"].map(|domain| observe_domain(uid, domain, &mut run))
    }

    fn observe_domain(
        uid: u32,
        domain: &str,
        run: impl FnOnce(&mut Command, Duration) -> Result<ExitStatus, RunError>,
    ) -> Observation {
        let mut command = Command::new("/bin/launchctl");
        command.args(["print", &format!("{domain}/{uid}/com.handkite.host")]);
        protect_child(&mut command);
        match run(&mut command, TIMEOUT) {
            Ok(status) if status.code() == Some(0) => Observation::Loaded,
            Ok(status) if status.code() == Some(113) => Observation::Absent,
            _ => Observation::Unknown,
        }
    }

    fn protect_child(command: &mut Command) {
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum RunError {
        Spawn,
        Wait,
        Timeout,
    }

    pub(super) fn run_command(
        command: &mut Command,
        timeout: Duration,
    ) -> Result<ExitStatus, RunError> {
        let mut child = command.spawn().map_err(|_| RunError::Spawn)?;
        wait_child(&mut child, timeout)
    }

    trait ChildProcess {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
        fn kill(&mut self) -> io::Result<()>;
        fn wait(&mut self) -> io::Result<ExitStatus>;
    }

    impl ChildProcess for Child {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Child::try_wait(self)
        }
        fn kill(&mut self) -> io::Result<()> {
            Child::kill(self)
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            Child::wait(self)
        }
    }

    fn wait_child(
        child: &mut impl ChildProcess,
        timeout: Duration,
    ) -> Result<ExitStatus, RunError> {
        let deadline = Instant::now() + timeout;
        let failure = loop {
            if Instant::now() >= deadline {
                break RunError::Timeout;
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                ),
                Err(_) => break RunError::Wait,
            }
        };
        // Only the child created by this invocation is terminated and reaped.
        // Cleanup failure never promotes an unknown result into absence.
        let _ = child.kill();
        let _ = child.wait();
        Err(failure)
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(all(test, not(target_os = "macos")))]
mod platform_tests {
    #[test]
    fn unsupported_platform_returns_unknown() {
        assert_eq!(super::observe(), super::Observation::Unknown);
    }
}
