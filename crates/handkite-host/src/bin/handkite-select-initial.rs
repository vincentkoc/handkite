//! Select an already published first release; never sign, activate, or send UI input.

use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    device_agreement::DeviceAgreement,
    installer_lock::InstallerLock,
    release_stage::reopen_published,
    selection_intent::{self, InitialSelectionStage},
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
    Selected,
    Invalid,
    SetupRejected,
    ReleaseRejected,
    DeviceRejected,
    Failed {
        stage: InitialSelectionStage,
        may_be_applied: bool,
        residue: bool,
    },
}

fn main() -> ExitCode {
    let outcome = if !install_input::arguments(std::env::args_os().skip(1)) {
        Outcome::Invalid
    } else {
        let binding_fd = unsafe { libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 4) };
        if binding_fd < 0 {
            Outcome::Invalid
        } else {
            let mut binding = unsafe { File::from_raw_fd(binding_fd) };
            match io::stdin().as_fd().try_clone_to_owned() {
                Ok(fd) => install_input::run(&mut File::from(fd), &mut binding, execute)
                    .unwrap_or(Outcome::Invalid),
                Err(_) => Outcome::Invalid,
            }
        }
    };
    ExitCode::from(report(outcome, &mut io::stdout().lock()))
}

fn execute(pins: release_input::Envelope<'_>, expected: &DeviceBinding) -> Outcome {
    let Ok(layout) = ServiceLayout::current_account() else {
        return Outcome::SetupRejected;
    };
    let Ok(owner) = InstallerLock::acquire(&layout) else {
        return Outcome::SetupRejected;
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
        let Ok(agreement) = DeviceAgreement::observe(&owner, expected).await else {
            return Outcome::DeviceRejected;
        };
        match selection_intent::select_initial(&owner, agreement, &mut release).await {
            Ok(()) => Outcome::Selected,
            Err(failure) => Outcome::Failed {
                stage: failure.stage(),
                may_be_applied: failure.selection_may_be_applied(),
                residue: failure.has_residue(),
            },
        }
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, message) = match outcome {
        Outcome::Selected => (0, "initial_select=selected\n".into()),
        Outcome::Invalid => (2, "initial_select=invalid\n".into()),
        Outcome::SetupRejected => (1, "initial_select=reject stage=setup\n".into()),
        Outcome::ReleaseRejected => (1, "initial_select=reject stage=release\n".into()),
        Outcome::DeviceRejected => (1, "initial_select=reject stage=device_agreement\n".into()),
        Outcome::Failed {
            stage,
            may_be_applied,
            residue,
        } => {
            let stage = match stage {
                InitialSelectionStage::Preparation => "preparation",
                InitialSelectionStage::Commit => "commit",
                InitialSelectionStage::Retirement => "retirement",
                InitialSelectionStage::Verification => "verification",
            };
            (
                if may_be_applied { 3 } else { 1 },
                format!(
                    "initial_select={} stage={stage} residue={}\n",
                    if may_be_applied { "unknown" } else { "reject" },
                    if residue { "present" } else { "absent" },
                ),
            )
        }
    };
    if output.write_all(message.as_bytes()).is_err() {
        return 3;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_or_uncertain_selection_is_never_reported_as_rejection() {
        for stage in [
            InitialSelectionStage::Commit,
            InitialSelectionStage::Retirement,
            InitialSelectionStage::Verification,
        ] {
            let mut output = Vec::new();
            assert_eq!(
                report(
                    Outcome::Failed {
                        stage,
                        may_be_applied: true,
                        residue: true,
                    },
                    &mut output,
                ),
                3
            );
            let message = String::from_utf8(output).unwrap();
            assert!(message.starts_with("initial_select=unknown stage="));
            assert!(message.ends_with(" residue=present\n"));
        }
    }

    #[test]
    fn failed_success_receipt_is_unknown_and_does_not_retry_output() {
        struct Broken(usize);
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Broken(0);
        assert_eq!(report(Outcome::Selected, &mut output), 3);
        assert_eq!(output.0, 1);
    }

    #[test]
    fn fixed_success_and_preselection_failures_have_distinct_exit_codes() {
        for (outcome, expected) in [
            (Outcome::Selected, 0),
            (Outcome::Invalid, 2),
            (Outcome::SetupRejected, 1),
            (Outcome::ReleaseRejected, 1),
            (Outcome::DeviceRejected, 1),
        ] {
            let mut output = Vec::new();
            assert_eq!(report(outcome, &mut output), expected);
            assert!(output.ends_with(b"\n"));
        }
    }
}
