use handkite_core::protocol::{HardwareId, LockState};
use serde_json::Value;
use std::ffi::CStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
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

struct QueryDirectory(PathBuf);

impl QueryDirectory {
    fn new() -> Option<Self> {
        let mut template = b"/tmp/handkite-lock-XXXXXX\0".to_vec();
        // mkdtemp replaces the X suffix in place and creates a private directory atomically.
        let pointer = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
        if pointer.is_null() {
            return None;
        }
        let path = unsafe { CStr::from_ptr(pointer) }.to_str().ok()?;
        Some(Self(PathBuf::from(path)))
    }

    fn output(&self) -> PathBuf {
        self.0.join("lock.json")
    }
}

impl Drop for QueryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.output());
        let _ = fs::remove_dir(&self.0);
    }
}

fn parse(bytes: &[u8]) -> Option<NativeLock> {
    if bytes.len() > MAX_JSON_BYTES {
        return None;
    }
    let json: Value = serde_json::from_slice(bytes).ok()?;
    if json["info"]["outcome"] != "success"
        || !json["info"]["commandType"].as_str()?.contains("lockState")
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
    Storage,
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
            Self::Storage => "native_lock_storage",
            Self::Spawn => "native_lock_spawn",
            Self::Wait => "native_lock_wait",
            Self::Exit => "native_lock_exit",
            Self::Read => "native_lock_read",
            Self::Decode => "native_lock_decode",
        }
    }
}

fn query_command(id: &HardwareId, output: &Path) -> Command {
    let mut command = Command::new("/usr/bin/xcrun");
    command
        .args(["devicectl", "device", "info", "lockState", "--device"])
        .arg(id.as_str())
        .args(["--timeout", "5", "--json-output"])
        .arg(output)
        .arg("--quiet")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn query_inner(id: &HardwareId, deadline: Instant) -> Result<NativeLock, QueryFailure> {
    if Instant::now() >= deadline {
        return Err(QueryFailure::Deadline);
    }
    let directory = QueryDirectory::new().ok_or(QueryFailure::Storage)?;
    if Instant::now() >= deadline {
        return Err(QueryFailure::Deadline);
    }
    let mut child = {
        let _timing = crate::timing::Span::new("native_lock_spawn");
        query_command(id, &directory.output())
            .spawn()
            .map_err(|_| QueryFailure::Spawn)?
    };
    {
        let _timing = crate::timing::Span::new("native_lock_process");
        wait_until(&mut child, deadline)?;
    }
    let _timing = crate::timing::Span::new("native_lock_read_decode");
    let mut bytes = Vec::new();
    fs::File::open(directory.output())
        .map_err(|_| QueryFailure::Read)?
        .take(MAX_JSON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| QueryFailure::Read)?;
    parse(&bytes).ok_or(QueryFailure::Decode)
}

fn wait_until(child: &mut Child, deadline: Instant) -> Result<(), QueryFailure> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if Instant::now() >= deadline {
                    Err(QueryFailure::Deadline)
                } else if status.success() {
                    Ok(())
                } else {
                    Err(QueryFailure::Exit)
                };
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(QueryFailure::Wait);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(50)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(QueryFailure::Deadline);
            }
        }
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
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        assert_eq!(
            wait_until(&mut child, Instant::now()),
            Err(QueryFailure::Deadline)
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn failed_native_process_is_distinct_from_deadline_expiry() {
        let mut child = Command::new("/usr/bin/false").spawn().unwrap();
        assert_eq!(
            wait_until(&mut child, Instant::now() + Duration::from_secs(2)),
            Err(QueryFailure::Exit)
        );
    }

    #[test]
    fn native_query_uses_fixed_tool_and_literal_arguments() {
        let id = HardwareId::new("test-device").unwrap();
        let command = query_command(&id, Path::new("/tmp/test-output/lock.json"));
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
                "/tmp/test-output/lock.json",
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
}
