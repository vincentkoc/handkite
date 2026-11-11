//! Stage-only signature checking, not path stability or release acceptance.

use crate::release_manifest::ArtifactName;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_CERTIFICATE: &str = "invalid signer certificate SHA1";
const REJECTED: &str = "artifact signature does not satisfy signer policy";

/// Explicit caller trust policy, independent of the candidate manifest.
pub struct SignerPolicy {
    requirement: String,
}

impl SignerPolicy {
    /// The certificate must come from a separately trusted source. No identity
    /// lookup, default signer, or certificate supplied by a manifest is accepted.
    pub fn new(certificate_sha1: &str, artifact: ArtifactName) -> Result<Self, &'static str> {
        if certificate_sha1.len() != 40
            || !certificate_sha1
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(INVALID_CERTIFICATE);
        }
        Ok(Self {
            requirement: format!(
                "=anchor apple generic and certificate leaf = H\"{certificate_sha1}\" and identifier \"{}\"",
                artifact.code_identifier()
            ),
        })
    }

    /// Check the code signature at an absolute candidate path on macOS.
    ///
    /// Success applies only to this tool invocation. The caller must separately
    /// anchor file access, prevent replacement, and compare bytes with a trusted
    /// manifest. This does not install code or qualify notarization, Gatekeeper,
    /// certificate revocation, entitlements, or release acceptance.
    pub fn check(&self, candidate: &Path) -> Result<(), &'static str> {
        if !cfg!(target_os = "macos") {
            return Err("signature checking requires macOS");
        }
        self.check_with(candidate, run_command)
    }

    pub(crate) fn check_with(
        &self,
        candidate: &Path,
        run: impl FnOnce(&mut Command, Duration) -> Result<ExitStatus, RunError>,
    ) -> Result<(), &'static str> {
        // codesign interprets decimal and +decimal operands as process IDs.
        // An absolute path also prevents option injection without resolving it.
        if !candidate.is_absolute() {
            return Err("signature check requires an absolute candidate path");
        }
        let mut command = Command::new("/usr/bin/codesign");
        command
            .env_clear()
            .args([
                "--verify",
                "--strict=all",
                "--all-architectures",
                "--test-requirement",
            ])
            .arg(&self.requirement)
            .arg("--")
            .arg(candidate);
        match run(&mut command, CHECK_TIMEOUT) {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(REJECTED),
            Err(RunError::Unavailable) => Err("signature check unavailable"),
            Err(RunError::Timeout) => Err("signature check timed out"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunError {
    Unavailable,
    Timeout,
}

pub(crate) fn run_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<ExitStatus, RunError> {
    // Capture zero output bytes: tool diagnostics may contain candidate paths.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RunError::Unavailable)?;
    let deadline = Instant::now() + timeout;
    let failure = loop {
        if Instant::now() >= deadline {
            break RunError::Timeout;
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break RunError::Unavailable,
        }
    };
    // Only this invocation's child is terminated; reap it before returning.
    let _ = child.kill();
    let _ = child.wait();
    Err(failure)
}

#[cfg(test)]
mod tests;
