use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Config(PathBuf);

impl Config {
    fn new(key: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "handkite-unlock-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(file, r#"{{"device_key":"{key}","usb_id":"fixture-usb"}}"#).unwrap();
        Self(path)
    }

    fn run(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_handkite-unlock-once"))
            .arg("--config")
            .arg(&self.0)
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

#[test]
fn caller_state_directory_is_rejected_before_pin_input() {
    let config = Config::new("iphone");
    for extra in [
        vec!["--state-dir", "/unused/fresh-state"],
        vec![
            "--state-dir",
            "/unused/fresh-state",
            "--supervised-visibility-fallback",
        ],
        vec!["--state-dir=/unused/fresh-state"],
    ] {
        let output = config.run(&extra);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "handkite-unlock-once: invalid_arguments\n"
        );
    }
}

#[test]
fn owned_session_flags_refuse_before_config_state_or_input() {
    for extra in [
        vec!["--owned-wda-session"],
        vec!["--owned-wda-session", "--temporary-idle-settings"],
        vec!["--owned-wda-session", "--supervised-visibility-fallback"],
        vec![
            "--owned-wda-session",
            "--temporary-idle-settings",
            "--supervised-visibility-fallback",
            "--owned-wda-session",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_handkite-unlock-once"))
            .args(["--config", "/unused/missing-owned-session-config"])
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "handkite-unlock-once: invalid_arguments\n"
        );
    }
}

#[test]
fn alternate_device_keys_are_rejected_before_state_or_pin_input() {
    for key in ["phone", "iphone-15-pro", "other"] {
        let config = Config::new(key);
        for extra in [vec![], vec!["--supervised-visibility-fallback"]] {
            let output = config.run(&extra);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                "handkite-unlock-once: PIN entry requires the fixed device key iphone\n"
            );
        }
    }
}
