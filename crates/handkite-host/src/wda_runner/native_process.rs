use super::RunnerError;
use crate::usb::PinnedUsb;
use handkite_core::protocol::HardwareId;
use serde_json::Value;
use std::ffi::CStr;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tokio::time::{Duration, Instant, sleep, timeout_at};

const MAX_JSON: u64 = 1024 * 1024;

#[derive(Clone)]
pub(super) struct Target {
    device: HardwareId,
    executable: PathBuf,
}

#[derive(Clone)]
pub(super) struct Identity {
    pub target: Target,
    pub pid: u32,
}

impl Target {
    pub fn new(device: HardwareId, app: &str, executable: &str) -> Result<Self, RunnerError> {
        let app = absolute_path(app).ok_or(RunnerError::ProcessIdentityUnproven)?;
        if executable.is_empty()
            || executable.chars().any(char::is_control)
            || Path::new(executable).components().count() != 1
            || !matches!(
                Path::new(executable).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(RunnerError::ProcessIdentityUnproven);
        }
        Ok(Self {
            device,
            executable: app.join(executable),
        })
    }

    pub async fn observe(
        &self,
        usb: &PinnedUsb,
        deadline: Instant,
        owns: &(dyn Fn() -> bool + Send + Sync),
        retained_pid: Option<u32>,
    ) -> Result<Option<u32>, RunnerError> {
        if !owns() {
            return Err(RunnerError::OwnershipLost);
        }
        timeout_at(deadline, usb.provider())
            .await
            .map_err(|_| RunnerError::ProcessIdentityUnproven)?
            .map_err(|_| RunnerError::ProcessIdentityUnproven)?;
        let json = command(&self.device, None, deadline, owns)
            .await
            .map_err(|error| {
                if error == RunnerError::OwnershipLost {
                    error
                } else {
                    RunnerError::ProcessIdentityUnproven
                }
            })?;
        select(&json, &self.executable, retained_pid)
    }
}

impl Identity {
    pub async fn stop_once(
        &self,
        usb: &PinnedUsb,
        deadline: Instant,
        owns: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<bool, RunnerError> {
        self.stop_with(
            self.target.observe(usb, deadline, owns, Some(self.pid)),
            async {
                command(&self.target.device, Some(self.pid), deadline, owns).await?;
                Ok(())
            },
        )
        .await
    }

    async fn stop_with(
        &self,
        observation: impl std::future::Future<Output = Result<Option<u32>, RunnerError>>,
        signal: impl std::future::Future<Output = Result<(), RunnerError>>,
    ) -> Result<bool, RunnerError> {
        match observation.await? {
            None => Ok(false),
            Some(pid) if pid == self.pid => {
                // No birth token is exposed by this API. Exact fresh PID/path
                // and exclusive ownership narrow, but cannot remove, PID reuse
                // between this observation and the single signal request.
                signal.await?;
                Ok(true)
            }
            _ => Err(RunnerError::ProcessIdentityUnproven),
        }
    }
}

fn absolute_path(value: &str) -> Option<PathBuf> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let path = if value.starts_with("file:") {
        let url = reqwest::Url::parse(value).ok()?;
        if url.scheme() != "file"
            || url.host_str().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        url.to_file_path().ok()?
    } else {
        PathBuf::from(value)
    };
    (path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_))))
    .then_some(path)
}

fn select(
    json: &Value,
    executable: &Path,
    retained_pid: Option<u32>,
) -> Result<Option<u32>, RunnerError> {
    if json["info"]["outcome"] != "success"
        || json["info"]["commandType"] != "devicectl.device.info.processes"
    {
        return Err(RunnerError::ProcessIdentityUnproven);
    }
    let rows = json["result"]["runningProcesses"]
        .as_array()
        .ok_or(RunnerError::ProcessIdentityUnproven)?;
    let mut selected = None;
    let mut pids = std::collections::HashSet::new();
    for row in rows {
        let pid = row["processIdentifier"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .ok_or(RunnerError::ProcessIdentityUnproven)?;
        let path = row["executable"]
            .as_str()
            .and_then(absolute_path)
            .ok_or(RunnerError::ProcessIdentityUnproven)?;
        if !pids.insert(pid) || retained_pid == Some(pid) && path != executable {
            return Err(RunnerError::ProcessIdentityUnproven);
        }
        if path == executable {
            if selected.replace(pid).is_some() || retained_pid.is_some_and(|old| old != pid) {
                return Err(RunnerError::ProcessIdentityUnproven);
            }
        }
    }
    Ok(selected)
}

struct Output(PathBuf);
impl Output {
    fn new() -> Result<Self, RunnerError> {
        let mut template = b"/tmp/handkite-process-XXXXXX\0".to_vec();
        let raw = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
        if raw.is_null() {
            return Err(RunnerError::RunnerStopUnproven);
        }
        let path = unsafe { CStr::from_ptr(raw) }
            .to_str()
            .map_err(|_| RunnerError::RunnerStopUnproven)?;
        Ok(Self(PathBuf::from(path)))
    }
    fn file(&self) -> PathBuf {
        self.0.join("result.json")
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.file());
        let _ = fs::remove_dir(&self.0);
    }
}

struct RunningCommand(Child);
impl Drop for RunningCommand {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn argv(device: &HardwareId, pid: Option<u32>, output: &Path) -> Command {
    let mut command = Command::new("/usr/bin/xcrun");
    command.arg("devicectl").arg("device");
    if let Some(pid) = pid {
        command
            .args(["process", "terminate", "--pid"])
            .arg(pid.to_string());
    } else {
        command.args(["info", "processes"]);
    }
    command
        .arg("--device")
        .arg(device.as_str())
        .args(["--timeout", "3", "--json-output"])
        .arg(output)
        .arg("--quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

async fn command(
    device: &HardwareId,
    pid: Option<u32>,
    deadline: Instant,
    owns: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Value, RunnerError> {
    let output = Output::new()?;
    let mut command = argv(device, pid, &output.file());
    // This is the final admission check immediately before the native write.
    if !owns() {
        return Err(RunnerError::OwnershipLost);
    }
    if Instant::now() >= deadline {
        return Err(RunnerError::RunnerStopUnproven);
    }
    let mut child = RunningCommand(
        command
            .spawn()
            .map_err(|_| RunnerError::RunnerStopUnproven)?,
    );
    loop {
        if !owns() {
            return Err(RunnerError::OwnershipLost);
        }
        if Instant::now() >= deadline {
            return Err(RunnerError::RunnerStopUnproven);
        }
        match child.0.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return Err(RunnerError::RunnerStopUnproven),
            Ok(None) => sleep(Duration::from_millis(20)).await,
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(output.file())
        .and_then(|file| file.take(MAX_JSON + 1).read_to_end(&mut bytes))
        .map_err(|_| RunnerError::RunnerStopUnproven)?;
    if bytes.len() as u64 > MAX_JSON {
        return Err(RunnerError::RunnerStopUnproven);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| RunnerError::RunnerStopUnproven)?;
    let expected_command = if pid.is_some() {
        "devicectl.device.process.terminate"
    } else {
        "devicectl.device.info.processes"
    };
    if value["info"]["outcome"] != "success" || value["info"]["commandType"] != expected_command {
        return Err(RunnerError::RunnerStopUnproven);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> Target {
        Target::new(
            HardwareId::new("fixture-device").unwrap(),
            "/apps/Candidate.app",
            "Candidate-Runner",
        )
        .unwrap()
    }

    #[test]
    fn installed_identity_cannot_escape_its_app_path() {
        for (app, exe) in [
            ("relative.app", "Runner"),
            ("/apps/App.app", "../Runner"),
            ("/apps/App.app", "/Runner"),
            ("/apps/App.app", "."),
            ("file://other-host/apps/App.app", "Runner"),
        ] {
            assert!(Target::new(HardwareId::new("fixture-device").unwrap(), app, exe).is_err());
        }
    }

    fn processes(rows: Value) -> Value {
        json!({"info":{"outcome":"success","commandType":"devicectl.device.info.processes"},"result":{"deviceIdentifier":"fixture-device","runningProcesses":rows}})
    }

    #[test]
    fn identity_requires_unique_full_path_and_same_retained_pid() {
        let target = target();
        let rows = processes(json!([
            {"processIdentifier":10,"executable":"/apps/Other.app/Candidate-Runner"},
            {"processIdentifier":20,"executable":"file:///apps/Candidate.app/Candidate-Runner"}
        ]));
        assert_eq!(select(&rows, &target.executable, None), Ok(Some(20)));
        assert_eq!(select(&rows, &target.executable, Some(20)), Ok(Some(20)));
        assert!(select(&rows, &target.executable, Some(10)).is_err());
        assert!(select(&rows, &target.executable, Some(30)).is_err());
        let other = processes(
            json!([{ "processIdentifier":10,"executable":"/apps/Other.app/Candidate-Runner" }]),
        );
        assert_eq!(select(&other, &target.executable, None), Ok(None));
        assert_eq!(select(&other, &target.executable, Some(20)), Ok(None));
        let duplicate = processes(json!([
            {"processIdentifier":20,"executable":"/apps/Candidate.app/Candidate-Runner"},
            {"processIdentifier":21,"executable":"/apps/Candidate.app/Candidate-Runner"}
        ]));
        assert!(select(&duplicate, &target.executable, None).is_err());
    }

    #[test]
    fn incomplete_native_inventory_never_proves_absence() {
        let target = target();
        for rows in [
            json!(null),
            json!([{"processIdentifier":0,"executable":"/apps/Other.app/Other"}]),
            json!([{"processIdentifier":20}]),
            json!([{"processIdentifier":20,"executable":"relative/path"}]),
            json!([{"processIdentifier":20,"executable":"file://other-host/apps/Candidate.app/Candidate-Runner"}]),
            json!([{"processIdentifier":20,"executable":"/apps/../Candidate.app/Candidate-Runner"}]),
            json!([{"processIdentifier":20,"executable":"/apps/Other.app/Other"},{"processIdentifier":20,"executable":"/apps/Candidate.app/Candidate-Runner"}]),
        ] {
            assert!(select(&processes(rows), &target.executable, None).is_err());
        }
        assert!(select(&json!({}), &target.executable, None).is_err());
        assert_eq!(
            select(&processes(json!([])), &target.executable, None),
            Ok(None)
        );
    }

    #[test]
    fn native_commands_pin_target_and_never_escalate_signal() {
        let target = target();
        for pid in [None, Some(20)] {
            let command = argv(&target.device, pid, Path::new("/tmp/fixture/result.json"));
            assert_eq!(command.get_program(), "/usr/bin/xcrun");
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(
                &args[..3],
                if pid.is_some() {
                    &["devicectl", "device", "process"]
                } else {
                    &["devicectl", "device", "info"]
                }
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--device", "fixture-device"])
            );
            assert!(!args.contains(&"--kill"));
            if pid.is_some() {
                assert!(args.windows(2).any(|pair| pair == ["--pid", "20"]));
            }
        }
    }

    #[tokio::test]
    async fn stop_sends_once_only_for_exact_identity_and_never_retries_uncertainty() {
        use std::cell::Cell;
        let identity = Identity {
            target: target(),
            pid: 20,
        };
        for case in 0..5 {
            let sent = Cell::new(0);
            let result = identity
                .stop_with(
                    async {
                        match case {
                            0 => Ok(None),
                            1 | 4 => Ok(Some(20)),
                            2 => Ok(Some(21)),
                            _ => Err(RunnerError::OwnershipLost),
                        }
                    },
                    async {
                        sent.set(sent.get() + 1);
                        if case == 4 {
                            Err(RunnerError::RunnerStopUnproven)
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            assert_eq!(sent.get(), usize::from(case == 1 || case == 4));
            assert_eq!(
                result,
                match case {
                    0 => Ok(false),
                    1 => Ok(true),
                    2 => Err(RunnerError::ProcessIdentityUnproven),
                    3 => Err(RunnerError::OwnershipLost),
                    _ => Err(RunnerError::RunnerStopUnproven),
                }
            );
        }
    }

    #[test]
    fn cancellation_guard_reaps_only_its_own_command() {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        drop(RunningCommand(child));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
