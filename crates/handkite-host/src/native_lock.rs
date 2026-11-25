use handkite_core::protocol::{HardwareId, LockState};
use serde_json::Value;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_JSON_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLock {
    pub state: LockState,
    pub unlocked_since_boot: Option<bool>,
}

impl NativeLock {
    fn uncertain() -> Self {
        Self {
            state: LockState::Uncertain,
            unlocked_since_boot: None,
        }
    }
}

fn parse(bytes: &[u8]) -> Option<NativeLock> {
    if bytes.len() > MAX_JSON_BYTES {
        return None;
    }
    let json: Value = serde_json::from_slice(bytes).ok()?;
    if json["info"]["outcome"] != "success"
        || json["info"]["commandType"] != "devicectl.device.info.lockState"
    {
        return None;
    }
    let required = json["result"]["passcodeRequired"].as_bool()?;
    let unlocked_since_boot = json["result"]["unlockedSinceBoot"].as_bool()?;
    Some(NativeLock {
        state: if required {
            LockState::Locked
        } else {
            LockState::Unlocked
        },
        unlocked_since_boot: Some(unlocked_since_boot),
    })
}

pub fn query(id: &HardwareId) -> NativeLock {
    query_until(id, Instant::now() + Duration::from_secs(8))
}

pub(crate) fn query_until(id: &HardwareId, deadline: Instant) -> NativeLock {
    let _timing = crate::timing::Span::new("native_lock");
    let deadline = deadline.min(Instant::now() + Duration::from_secs(8));
    crate::timing::native_lock_budget(deadline.saturating_duration_since(Instant::now()));
    match query_inner(id, deadline) {
        Ok(value) => {
            crate::timing::outcome("native_lock_unlocked", value.state == LockState::Unlocked);
            value
        }
        Err(failure) => {
            crate::timing::outcome(failure.stage(), false);
            NativeLock::uncertain()
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum QueryFailure {
    Deadline,
    Spawn,
    Wait,
    Exit,
    Read,
    Decode,
}

impl QueryFailure {
    fn stage(&self) -> &'static str {
        match self {
            Self::Deadline => "native_lock_deadline",
            Self::Spawn => "native_lock_spawn",
            Self::Wait => "native_lock_wait",
            Self::Exit => "native_lock_exit",
            Self::Read => "native_lock_read",
            Self::Decode => "native_lock_decode",
        }
    }
}

fn query_command(id: &HardwareId) -> Command {
    let mut command = Command::new("/usr/bin/xcrun");
    command
        .args(["devicectl", "device", "info", "lockState", "--device"])
        .arg(id.as_str())
        .args(["--timeout", "5", "--json-output", "-", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn query_inner(id: &HardwareId, deadline: Instant) -> Result<NativeLock, QueryFailure> {
    if Instant::now() >= deadline {
        return Err(QueryFailure::Deadline);
    }
    let mut child = {
        let _timing = crate::timing::Span::new("native_lock_spawn");
        query_command(id).spawn().map_err(|_| QueryFailure::Spawn)?
    };
    let bytes = {
        let _timing = crate::timing::Span::new("native_lock_process");
        collect_until(&mut child, deadline)?
    };
    let _timing = crate::timing::Span::new("native_lock_read_decode");
    let state = parse(&bytes).ok_or(QueryFailure::Decode)?;
    if Instant::now() >= deadline {
        return Err(QueryFailure::Deadline);
    }
    Ok(state)
}

struct ReapChild<'a>(&'a mut Child);

impl Drop for ReapChild<'_> {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn collect_until(child: &mut Child, deadline: Instant) -> Result<Vec<u8>, QueryFailure> {
    let owned = ReapChild(child);
    let mut stdout = owned.0.stdout.take().ok_or(QueryFailure::Read)?;
    let fd = stdout.as_raw_fd();
    // Nonblocking reads let one loop drain the pipe and enforce the same
    // absolute deadline, without a detached reader or a pipe-capacity deadlock.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(QueryFailure::Read);
    }
    drain_until(owned.0, &mut stdout, deadline)
}

fn drain_until(
    child: &mut Child,
    reader: &mut impl Read,
    deadline: Instant,
) -> Result<Vec<u8>, QueryFailure> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut eof = false;
    let mut status = None;
    loop {
        if Instant::now() >= deadline {
            return Err(QueryFailure::Deadline);
        }
        while !eof {
            if Instant::now() >= deadline {
                return Err(QueryFailure::Deadline);
            }
            match reader.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    if count > MAX_JSON_BYTES - output.len() {
                        return Err(QueryFailure::Read);
                    }
                    output.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(QueryFailure::Read),
            }
        }
        if status.is_none() {
            status = child.try_wait().map_err(|_| QueryFailure::Wait)?;
        }
        if let Some(status) = status {
            if !status.success() {
                return Err(QueryFailure::Exit);
            }
            if eof {
                return if Instant::now() < deadline {
                    Ok(output)
                } else {
                    Err(QueryFailure::Deadline)
                };
            }
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_native_query_returns_uncertain_without_device_access() {
        let id = HardwareId::new("test-device").unwrap();
        assert_eq!(query_until(&id, Instant::now()), NativeLock::uncertain());
    }

    #[test]
    fn expired_native_wait_reaps_only_its_own_child() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(
            collect_until(&mut child, Instant::now()),
            Err(QueryFailure::Deadline)
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn failed_native_process_is_distinct_from_deadline_expiry() {
        let mut child = Command::new("/usr/bin/false")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(
            collect_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Exit)
        );
    }

    #[test]
    fn native_query_uses_fixed_tool_and_literal_arguments() {
        let id = HardwareId::new("test-device").unwrap();
        let command = query_command(&id);
        assert_eq!(command.get_program(), "/usr/bin/xcrun");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "devicectl",
                "device",
                "info",
                "lockState",
                "--device",
                "test-device",
                "--timeout",
                "5",
                "--json-output",
                "-",
                "--quiet",
            ]
        );
    }

    #[test]
    fn accepts_only_successful_typed_lock_results() {
        let source = br#"{"info":{"outcome":"success","commandType":"devicectl.device.info.lockState"},"result":{"passcodeRequired":true,"unlockedSinceBoot":false}}"#;
        assert_eq!(
            parse(source),
            Some(NativeLock {
                state: LockState::Locked,
                unlocked_since_boot: Some(false)
            })
        );
        let unlocked = String::from_utf8(source.to_vec())
            .unwrap()
            .replace("\"passcodeRequired\":true", "\"passcodeRequired\":false");
        assert_eq!(
            parse(unlocked.as_bytes()).unwrap().state,
            LockState::Unlocked
        );
        assert!(parse(br#"{"result":{"passcodeRequired":true}}"#).is_none());
        assert!(parse(&vec![b' '; MAX_JSON_BYTES + 1]).is_none());
    }

    const VALID: &str = r#"{"info":{"outcome":"success","commandType":"devicectl.device.info.lockState"},"result":{"passcodeRequired":false,"unlockedSinceBoot":true}}"#;

    fn printing_child(body: &str, exit: i32) -> Child {
        Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$1\"; exit \"$2\"",
                "fixture",
                body,
                &exit.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn drains_valid_output_larger_than_pipe_capacity_and_accepts_exact_limit() {
        for size in [32 * 1024, MAX_JSON_BYTES] {
            let mut body = VALID.to_owned();
            body.extend(std::iter::repeat_n(' ', size - body.len()));
            let mut child = printing_child(&body, 0);
            let bytes = collect_until(&mut child, Instant::now() + Duration::from_secs(2)).unwrap();
            assert_eq!(bytes.len(), size);
            assert_eq!(parse(&bytes).unwrap().state, LockState::Unlocked);
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    fn oversized_and_continuous_output_refuse_and_reap_the_writer() {
        let body = "x".repeat(MAX_JSON_BYTES + 1);
        let mut child = printing_child(&body, 0);
        assert_eq!(
            collect_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Read)
        );
        assert!(child.try_wait().unwrap().is_some());

        let mut child = Command::new("/usr/bin/yes")
            .arg("x")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(
            collect_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Read)
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn partial_output_and_early_eof_cannot_complete_before_child_exit() {
        for script in [
            "printf '{'; exec /bin/sleep 30",
            "exec 1>&-; exec /bin/sleep 30",
        ] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            assert_eq!(
                collect_until(&mut child, Instant::now() + Duration::from_millis(30)),
                Err(QueryFailure::Deadline)
            );
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    fn valid_json_from_unsuccessful_process_is_refused() {
        let mut child = printing_child(VALID, 1);
        assert_eq!(
            collect_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Exit)
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn missing_pipe_and_read_failure_reap_only_the_owned_child() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(
            collect_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Read)
        );
        assert!(child.try_wait().unwrap().is_some());

        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture"))
            }
        }
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let owned = ReapChild(&mut child);
            assert_eq!(
                drain_until(
                    owned.0,
                    &mut Broken,
                    Instant::now() + Duration::from_secs(2)
                ),
                Err(QueryFailure::Read)
            );
        }
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn exact_type_and_boolean_fields_are_required() {
        for body in [
            VALID.replace("devicectl.device.info.lockState", "prefix.lockState"),
            VALID.replace(
                "\"passcodeRequired\":false",
                "\"passcodeRequired\":\"false\"",
            ),
            VALID.replace(",\"unlockedSinceBoot\":true", ""),
            VALID.replace("success", "failed"),
        ] {
            assert!(parse(body.as_bytes()).is_none());
        }
    }
}
