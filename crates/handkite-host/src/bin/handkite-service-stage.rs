//! Explicit first-time descriptor staging; no release selection or service activation.

use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    install_config::RetainedConfig,
    installer_lock::InstallerLock,
    launchd_descriptor::{self, StagePhase},
    release_stage::reopen_published,
    service_layout::ServiceLayout,
};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::process::ExitCode;

#[path = "release_input/mod.rs"]
mod release_input;

#[path = "device_agreement/input.rs"]
mod binding_input;

#[path = "install_input/mod.rs"]
mod install_input;
use install_input::arguments;
#[cfg(test)]
use install_input::protected_read;

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

fn run(
    input: &mut File,
    expected: &mut File,
    stage: impl FnOnce(release_input::Envelope<'_>, &DeviceBinding) -> Outcome,
) -> Outcome {
    install_input::run(input, expected, stage).unwrap_or(Outcome::Invalid)
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
