//! Explicit upgrade or rollback of published releases; never activate or send input.

use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    device_agreement::DeviceAgreement,
    installer_lock::InstallerLock,
    release_stage::reopen_published,
    selection_intent::{self, SelectionPhase, SelectionStage, UpdateOperation},
    service_layout::ServiceLayout,
};
use std::ffi::{OsStr, OsString};
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
    PreviousRejected,
    DestinationRejected,
    DeviceRejected,
    Failed {
        stage: SelectionStage,
        phase: SelectionPhase,
        may_be_applied: bool,
        residue: bool,
    },
}

fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<UpdateOperation> {
    if args.next()?.as_os_str() != "--operation" {
        return None;
    }
    let operation = match args.next()?.to_str()? {
        "upgrade" => UpdateOperation::Upgrade,
        "rollback" => UpdateOperation::Rollback,
        _ => return None,
    };
    for expected in ["--binding-fd", "3", "--previous-fd", "4"] {
        if args.next()?.as_os_str() != OsStr::new(expected) {
            return None;
        }
    }
    args.next().is_none().then_some(operation)
}

fn duplicate(fd: i32) -> Option<File> {
    // Never occupy either original input fd while admitting the other one.
    let owned = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
    (owned >= 0).then(|| unsafe { File::from_raw_fd(owned) })
}

fn main() -> ExitCode {
    let outcome = (|| {
        let operation = arguments(std::env::args_os().skip(1))?;
        let mut binding = duplicate(3)?;
        let mut previous = duplicate(4)?;
        let mut destination = File::from(io::stdin().as_fd().try_clone_to_owned().ok()?);
        install_input::run_update(
            &mut destination,
            &mut binding,
            &mut previous,
            |to, from, binding| execute(operation, to, from, binding),
        )
    })()
    .unwrap_or(Outcome::Invalid);
    ExitCode::from(report(outcome, &mut io::stdout().lock()))
}

fn execute(
    operation: UpdateOperation,
    destination_pin: release_input::Envelope<'_>,
    previous_pin: release_input::Envelope<'_>,
    expected: &DeviceBinding,
) -> Outcome {
    let Ok(layout) = ServiceLayout::current_account() else {
        return Outcome::SetupRejected;
    };
    let Ok(owner) = InstallerLock::acquire(&layout) else {
        return Outcome::SetupRejected;
    };
    let Ok(mut previous) = reopen_published(
        &owner,
        previous_pin.manifest,
        &previous_pin.digest,
        previous_pin.certificate,
    ) else {
        return Outcome::PreviousRejected;
    };
    let Ok(mut destination) = reopen_published(
        &owner,
        destination_pin.manifest,
        &destination_pin.digest,
        destination_pin.certificate,
    ) else {
        return Outcome::DestinationRejected;
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
        match selection_intent::select_update(
            &owner,
            agreement,
            operation,
            &mut previous,
            &mut destination,
        )
        .await
        {
            Ok(()) => Outcome::Selected,
            Err(failure) => Outcome::Failed {
                stage: failure.stage(),
                phase: failure.phase(),
                may_be_applied: failure.selection_may_be_applied(),
                residue: failure.has_residue(),
            },
        }
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, message) = match outcome {
        Outcome::Selected => (0, "update_select=selected\n".into()),
        Outcome::Invalid => (2, "update_select=invalid\n".into()),
        Outcome::SetupRejected => (1, "update_select=reject stage=setup\n".into()),
        Outcome::PreviousRejected => (1, "update_select=reject stage=previous_release\n".into()),
        Outcome::DestinationRejected => {
            (1, "update_select=reject stage=destination_release\n".into())
        }
        Outcome::DeviceRejected => (1, "update_select=reject stage=device_agreement\n".into()),
        Outcome::Failed {
            stage,
            phase,
            may_be_applied,
            residue,
        } => {
            let stage = match stage {
                SelectionStage::Preparation => "preparation",
                SelectionStage::Commit => "commit",
                SelectionStage::Retirement => "retirement",
                SelectionStage::Verification => "verification",
            };
            let phase_name = match phase {
                SelectionPhase::Before => "before",
                SelectionPhase::Applied => "applied",
                SelectionPhase::Uncertain => "uncertain",
            };
            let applied = if !may_be_applied {
                "no"
            } else if stage == "commit" && phase == SelectionPhase::Uncertain {
                "unknown"
            } else {
                "yes"
            };
            (
                if may_be_applied { 3 } else { 1 },
                format!(
                    "update_select={} stage={stage} phase={phase_name} applied={applied} residue={}\n",
                    if may_be_applied { "unknown" } else { "reject" },
                    if residue { "present" } else { "absent" },
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
#[path = "select_update/tests.rs"]
mod tests;
