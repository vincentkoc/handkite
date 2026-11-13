//! Publish the fixed GUI LaunchAgent; never bootstrap or activate a service.

use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    device_agreement::DeviceAgreement,
    install_config::RetainedConfig,
    installer_lock::InstallerLock,
    launchd_descriptor::{self, RegistrationOutcome, StagePhase},
    release_stage::reopen_published,
    service_layout::ServiceLayout,
};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::process::ExitCode;

#[path = "device_agreement/input.rs"]
mod binding_input;
#[path = "install_input/mod.rs"]
mod install_input;
#[path = "release_input/mod.rs"]
mod release_input;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Complete(RegistrationOutcome),
    Invalid,
    SetupRejected,
    ConfigRejected,
    ReleaseRejected,
    DeviceRejected,
    Failed(StagePhase, bool),
}

fn main() -> ExitCode {
    let outcome = (|| {
        if !install_input::arguments(std::env::args_os().skip(1)) {
            return None;
        }
        let fd = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
        if fd < 0 {
            return None;
        }
        let mut binding = unsafe { File::from_raw_fd(fd) };
        let mut input = File::from(io::stdin().as_fd().try_clone_to_owned().ok()?);
        install_input::run(&mut input, &mut binding, execute)
    })()
    .unwrap_or(Outcome::Invalid);
    ExitCode::from(report(outcome, &mut io::stdout().lock()))
}

fn execute(pins: release_input::Envelope<'_>, binding: &DeviceBinding) -> Outcome {
    let Ok(layout) = ServiceLayout::current_account() else {
        return Outcome::SetupRejected;
    };
    let Ok(owner) = InstallerLock::acquire(&layout) else {
        return Outcome::SetupRejected;
    };
    let Ok(mut config) = RetainedConfig::acquire(&owner, binding) else {
        return Outcome::ConfigRejected;
    };
    let Ok(mut release) = reopen_published(&owner, pins.manifest, &pins.digest, pins.certificate)
    else {
        return Outcome::ReleaseRejected;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Outcome::SetupRejected;
    };
    runtime.block_on(async {
        let Ok(agreement) = DeviceAgreement::observe(&owner, binding).await else {
            return Outcome::DeviceRejected;
        };
        match launchd_descriptor::register(&owner, agreement, &mut config, &mut release).await {
            Ok(outcome) => Outcome::Complete(outcome),
            Err(error) => Outcome::Failed(error.phase(), error.has_residue()),
        }
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, message) = match outcome {
        Outcome::Complete(RegistrationOutcome::Registered) => {
            (0, "service_register=registered\n".into())
        }
        Outcome::Complete(RegistrationOutcome::VerifiedExisting) => {
            (0, "service_register=verified_existing\n".into())
        }
        Outcome::Invalid => (2, "service_register=invalid\n".into()),
        Outcome::SetupRejected => (1, "service_register=reject stage=setup\n".into()),
        Outcome::ConfigRejected => (1, "service_register=reject stage=config\n".into()),
        Outcome::ReleaseRejected => (1, "service_register=reject stage=release\n".into()),
        Outcome::DeviceRejected => (1, "service_register=reject stage=device_agreement\n".into()),
        Outcome::Failed(phase, residue) => {
            let (code, state, phase) = match phase {
                StagePhase::BeforeRename => (1, "reject", "before_rename"),
                StagePhase::AppliedRename => (3, "unknown", "applied_rename"),
                StagePhase::UncertainRename => (3, "unknown", "uncertain_rename"),
            };
            (
                code,
                format!(
                    "service_register={state} phase={phase} residue={}\n",
                    if residue { "present" } else { "absent" }
                ),
            )
        }
    };
    if output.write_all(message.as_bytes()).is_err() {
        3
    } else {
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_all_publication_boundaries_without_activation_claims() {
        for (outcome, code, expected) in [
            (
                Outcome::Complete(RegistrationOutcome::Registered),
                0,
                "service_register=registered\n",
            ),
            (
                Outcome::Complete(RegistrationOutcome::VerifiedExisting),
                0,
                "service_register=verified_existing\n",
            ),
            (Outcome::Invalid, 2, "service_register=invalid\n"),
            (
                Outcome::Failed(StagePhase::BeforeRename, true),
                1,
                "service_register=reject phase=before_rename residue=present\n",
            ),
            (
                Outcome::Failed(StagePhase::AppliedRename, true),
                3,
                "service_register=unknown phase=applied_rename residue=present\n",
            ),
            (
                Outcome::Failed(StagePhase::UncertainRename, true),
                3,
                "service_register=unknown phase=uncertain_rename residue=present\n",
            ),
        ] {
            let mut output = Vec::new();
            assert_eq!(report(outcome, &mut output), code);
            assert_eq!(output, expected.as_bytes());
        }
    }

    #[test]
    fn broken_output_is_unknown_without_retry() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            report(
                Outcome::Complete(RegistrationOutcome::Registered),
                &mut Broken
            ),
            3
        );
    }
}
