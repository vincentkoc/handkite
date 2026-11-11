//! Explicit first-time descriptor staging; no release selection or service activation.

use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    install_config::RetainedConfig,
    installer_lock::InstallerLock,
    launchd_descriptor::{self, StagePhase},
    release_manifest::{ArtifactName, MAX_MANIFEST_BYTES, ReleaseManifest},
    release_signer::SignerPolicy,
    release_stage::reopen_published,
    release_verify::check_private_input_acl,
    service_layout::ServiceLayout,
};
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Seek, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::process::ExitCode;

#[path = "release_input/mod.rs"]
mod release_input;

#[path = "device_agreement/input.rs"]
mod binding_input;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Staged,
    Invalid,
    SetupRejected,
    ConfigRejected,
    ReleaseRejected,
    Failed(StagePhase, bool),
}

impl Outcome {
    fn report(self) -> (u8, &'static str) {
        match self {
            Self::Staged => (0, "service_stage=staged\n"),
            Self::Invalid => (2, "service_stage=invalid\n"),
            Self::SetupRejected => (1, "service_stage=reject stage=setup\n"),
            Self::ConfigRejected => (1, "service_stage=reject stage=config\n"),
            Self::ReleaseRejected => (1, "service_stage=reject stage=release\n"),
            Self::Failed(StagePhase::BeforeRename, false) => (
                1,
                "service_stage=reject phase=before_rename residue=absent\n",
            ),
            Self::Failed(StagePhase::BeforeRename, true) => (
                1,
                "service_stage=reject phase=before_rename residue=present\n",
            ),
            Self::Failed(StagePhase::AppliedRename, false) => (
                3,
                "service_stage=unknown phase=applied_rename residue=absent\n",
            ),
            Self::Failed(StagePhase::AppliedRename, true) => (
                3,
                "service_stage=unknown phase=applied_rename residue=present\n",
            ),
            Self::Failed(StagePhase::UncertainRename, false) => (
                3,
                "service_stage=unknown phase=uncertain_rename residue=absent\n",
            ),
            Self::Failed(StagePhase::UncertainRename, true) => (
                3,
                "service_stage=unknown phase=uncertain_rename residue=present\n",
            ),
        }
    }
}

fn arguments(mut args: impl Iterator<Item = OsString>) -> bool {
    args.next().as_deref() == Some(std::ffi::OsStr::new("--binding-fd"))
        && args.next().as_deref() == Some(std::ffi::OsStr::new("3"))
        && args.next().is_none()
}

fn main() -> ExitCode {
    let outcome = if !arguments(std::env::args_os().skip(1)) {
        Outcome::Invalid
    } else {
        // Duplicate only the explicitly named fixed descriptor; never open a path.
        let binding_fd = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
        if binding_fd < 0 {
            Outcome::Invalid
        } else {
            let mut binding = unsafe { File::from_raw_fd(binding_fd) };
            match io::stdin().as_fd().try_clone_to_owned() {
                Ok(fd) => run(&mut File::from(fd), &mut binding, execute),
                Err(_) => Outcome::Invalid,
            }
        }
    };
    let (code, message) = outcome.report();
    if io::stdout().lock().write_all(message.as_bytes()).is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    (
        left.dev(),
        left.ino(),
        left.uid(),
        left.gid(),
        left.mode(),
        left.nlink(),
        left.len(),
        left.mtime(),
        left.mtime_nsec(),
        left.ctime(),
        left.ctime_nsec(),
    ) == (
        right.dev(),
        right.ino(),
        right.uid(),
        right.gid(),
        right.mode(),
        right.nlink(),
        right.len(),
        right.mtime(),
        right.mtime_nsec(),
        right.ctime(),
        right.ctime_nsec(),
    )
}

fn protected_read(
    input: &mut File,
    max: usize,
    read: impl FnOnce(&mut File) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let before = input.metadata().ok()?;
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > max as u64
        || input.stream_position().ok()? != 0
    {
        return None;
    }
    check_private_input_acl(input).ok()?;
    let bytes = read(input)?;
    if bytes.len() as u64 != before.len() || !same_metadata(&before, &input.metadata().ok()?) {
        return None;
    }
    check_private_input_acl(input).ok()?;
    Some(bytes)
}

fn run(
    input: &mut File,
    expected: &mut File,
    stage: impl FnOnce(release_input::Envelope<'_>, &DeviceBinding) -> Outcome,
) -> Outcome {
    let Ok(left) = input.metadata() else {
        return Outcome::Invalid;
    };
    let Ok(right) = expected.metadata() else {
        return Outcome::Invalid;
    };
    if (left.dev(), left.ino()) == (right.dev(), right.ino()) {
        return Outcome::Invalid;
    }
    let Some(bytes) = protected_read(input, MAX_MANIFEST_BYTES + 106, release_input::read_input)
    else {
        return Outcome::Invalid;
    };
    let Some(pins) = release_input::envelope(&bytes) else {
        return Outcome::Invalid;
    };
    let Ok(manifest) = ReleaseManifest::parse(pins.manifest) else {
        return Outcome::Invalid;
    };
    if manifest.canonical_json().ok().as_deref() != Some(pins.manifest)
        || ring::digest::digest(&ring::digest::SHA256, pins.manifest).as_ref() != pins.digest
        || SignerPolicy::new(pins.certificate, ArtifactName::Daemon).is_err()
    {
        return Outcome::Invalid;
    }
    let Some(expected_binding) = binding_input::read_binding(expected) else {
        return Outcome::Invalid;
    };
    if !expected
        .metadata()
        .is_ok_and(|after| same_metadata(&right, &after))
        || check_private_input_acl(expected).is_err()
    {
        return Outcome::Invalid;
    }
    stage(pins, &expected_binding)
}

fn execute(pins: release_input::Envelope<'_>, expected: &DeviceBinding) -> Outcome {
    let Ok(layout) = ServiceLayout::current_account() else {
        return Outcome::SetupRejected;
    };
    let Ok(owner) = InstallerLock::acquire(&layout) else {
        return Outcome::SetupRejected;
    };
    let Ok(mut config) = RetainedConfig::acquire(&owner, expected) else {
        return Outcome::ConfigRejected;
    };
    if config.wda_runner_bundle_id().is_none() {
        return Outcome::ConfigRejected;
    }
    let Ok(mut release) = reopen_published(&owner, pins.manifest, &pins.digest, pins.certificate)
    else {
        return Outcome::ReleaseRejected;
    };
    match launchd_descriptor::stage(&owner, &mut config, &mut release) {
        Ok(_descriptor) => Outcome::Staged,
        Err(error) => Outcome::Failed(error.phase(), error.has_residue()),
    }
}

#[cfg(test)]
#[path = "service_stage/tests.rs"]
mod tests;
