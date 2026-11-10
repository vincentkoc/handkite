//! Explicit signed release publication. This command never selects or starts a controller.

use handkite_host::{
    installer_lock::InstallerLock,
    release_stage::{self, PublicationPhase},
    service_layout::ServiceLayout,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::ExitCode;

#[path = "release_input/mod.rs"]
mod release_input;
use release_input::{envelope, read_input, stage_arg};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Published,
    Invalid,
    SetupRejected,
    StageRejected {
        residue: bool,
    },
    PublicationFailed {
        phase: PublicationPhase,
        residue: bool,
    },
    VerificationUnknown,
}

impl Outcome {
    fn report(self) -> (u8, &'static str) {
        match self {
            Self::Published => (0, "release_publish=published\n"),
            Self::Invalid => (2, "release_publish=invalid\n"),
            Self::SetupRejected => (1, "release_publish=reject stage=setup\n"),
            Self::StageRejected { residue: false } => {
                (1, "release_publish=reject stage=staging residue=absent\n")
            }
            Self::StageRejected { residue: true } => {
                (1, "release_publish=reject stage=staging residue=present\n")
            }
            Self::PublicationFailed {
                phase: PublicationPhase::BeforeCommit,
                residue: false,
            } => (
                1,
                "release_publish=reject stage=publication phase=before_commit residue=absent\n",
            ),
            Self::PublicationFailed {
                phase: PublicationPhase::BeforeCommit,
                residue: true,
            } => (
                1,
                "release_publish=reject stage=publication phase=before_commit residue=present\n",
            ),
            Self::PublicationFailed {
                phase: PublicationPhase::AfterCommit,
                residue: false,
            } => (
                3,
                "release_publish=unknown stage=publication phase=after_commit residue=absent\n",
            ),
            Self::PublicationFailed {
                phase: PublicationPhase::AfterCommit,
                residue: true,
            } => (
                3,
                "release_publish=unknown stage=publication phase=after_commit residue=present\n",
            ),
            Self::PublicationFailed {
                phase: PublicationPhase::UncertainCommit,
                residue: false,
            } => (
                3,
                "release_publish=unknown stage=publication phase=uncertain_commit residue=absent\n",
            ),
            Self::PublicationFailed {
                phase: PublicationPhase::UncertainCommit,
                residue: true,
            } => (
                3,
                "release_publish=unknown stage=publication phase=uncertain_commit residue=present\n",
            ),
            Self::VerificationUnknown => (3, "release_publish=unknown stage=verification\n"),
        }
    }
}

fn main() -> ExitCode {
    let outcome = match io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => run(
            std::env::args_os().skip(1),
            &mut File::from(fd),
            publish_release,
        ),
        Err(_) => Outcome::Invalid,
    };
    let (code, message) = outcome.report();
    if io::stdout().lock().write_all(message.as_bytes()).is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}

fn run(
    args: impl Iterator<Item = OsString>,
    input: &mut File,
    publish: impl FnOnce(&Path, &[u8], &[u8; 32], &str) -> Outcome,
) -> Outcome {
    let Some(source) = stage_arg(args) else {
        return Outcome::Invalid;
    };
    let Some(bytes) = read_input(input) else {
        return Outcome::Invalid;
    };
    let Some(pins) = envelope(&bytes) else {
        return Outcome::Invalid;
    };
    publish(&source, pins.manifest, &pins.digest, pins.certificate)
}

fn publish_release(
    source: &Path,
    manifest: &[u8],
    digest: &[u8; 32],
    certificate: &str,
) -> Outcome {
    let Ok(layout) = ServiceLayout::current_account() else {
        return Outcome::SetupRejected;
    };
    let Ok(owner) = InstallerLock::acquire(&layout) else {
        return Outcome::SetupRejected;
    };
    sequence(
        || {
            release_stage::stage(&owner, source, manifest, digest, certificate)
                .map_err(|error| error.residue_directory_name().is_some())
        },
        |staged| {
            release_stage::publish(staged).map_err(|error| {
                (
                    error.phase(),
                    !error.directory_residues().is_empty() || !error.receipt_residues().is_empty(),
                )
            })
        },
        |published| published.revalidate().map_err(|_| ()),
    )
}

fn sequence<S, P>(
    stage: impl FnOnce() -> Result<S, bool>,
    publish: impl FnOnce(S) -> Result<P, (PublicationPhase, bool)>,
    verify: impl FnOnce(&mut P) -> Result<(), ()>,
) -> Outcome {
    let staged = match stage() {
        Ok(staged) => staged,
        Err(residue) => return Outcome::StageRejected { residue },
    };
    let mut published = match publish(staged) {
        Ok(published) => published,
        Err((phase, residue)) => return Outcome::PublicationFailed { phase, residue },
    };
    if verify(&mut published).is_err() {
        return Outcome::VerificationUnknown;
    }
    Outcome::Published
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use tempfile::NamedTempFile;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CERTIFICATE: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn input(manifest: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(format!("{DIGEST}\n{CERTIFICATE}\n").as_bytes())
            .unwrap();
        file.write_all(manifest).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[test]
    fn exact_private_pins_reach_publication_once() {
        let source = b"/private/../stage//\xff";
        let manifest = b"{\n}\xff";
        let mut input = input(manifest);
        let calls = Cell::new(0);
        let outcome = run(
            ["--stage".into(), OsString::from_vec(source.to_vec())].into_iter(),
            input.as_file_mut(),
            |path, bytes, digest, certificate| {
                calls.set(calls.get() + 1);
                assert_eq!(path.as_os_str().as_bytes(), source);
                assert_eq!(bytes, manifest);
                assert_eq!(
                    digest,
                    &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
                        .repeat(4)
                        .as_slice()
                );
                assert_eq!(certificate, CERTIFICATE);
                Outcome::Published
            },
        );
        assert_eq!(outcome, Outcome::Published);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn invalid_arguments_and_input_never_enter_publication() {
        for arguments in [
            vec![],
            vec!["--stage", "relative"],
            vec!["--stage", "/stage", "--retry"],
        ] {
            let mut input = input(b"{}");
            assert_eq!(
                run(
                    arguments.into_iter().map(OsString::from),
                    input.as_file_mut(),
                    |_, _, _, _| panic!("publication called")
                ),
                Outcome::Invalid
            );
        }
        let mut invalid = NamedTempFile::new().unwrap();
        invalid.write_all(b"invalid").unwrap();
        invalid.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            run(
                ["--stage".into(), "/stage".into()].into_iter(),
                invalid.as_file_mut(),
                |_, _, _, _| panic!("publication called")
            ),
            Outcome::Invalid
        );
    }

    #[test]
    fn stage_publish_verify_are_single_ordered_consuming_steps() {
        let order = Cell::new(0);
        assert_eq!(
            sequence(
                || {
                    assert_eq!(order.replace(1), 0);
                    Ok::<_, bool>(7_u8)
                },
                |stage| {
                    assert_eq!(stage, 7);
                    assert_eq!(order.replace(2), 1);
                    Ok::<_, (PublicationPhase, bool)>(8_u8)
                },
                |published| {
                    assert_eq!(*published, 8);
                    assert_eq!(order.replace(3), 2);
                    Ok(())
                },
            ),
            Outcome::Published
        );
        assert_eq!(order.get(), 3);
    }

    #[test]
    fn failures_stop_at_the_first_failed_step() {
        assert_eq!(
            sequence(
                || Err::<u8, _>(true),
                |_: u8| -> Result<u8, (PublicationPhase, bool)> { panic!("publish called") },
                |_: &mut u8| panic!("verify called")
            ),
            Outcome::StageRejected { residue: true }
        );
        assert_eq!(
            sequence(
                || Ok::<_, bool>(1_u8),
                |_| Err::<u8, _>((PublicationPhase::UncertainCommit, true)),
                |_: &mut u8| panic!("verify called")
            ),
            Outcome::PublicationFailed {
                phase: PublicationPhase::UncertainCommit,
                residue: true
            }
        );
        assert_eq!(
            sequence(
                || Ok::<_, bool>(1_u8),
                |_| Ok::<_, (PublicationPhase, bool)>(2_u8),
                |_| Err(())
            ),
            Outcome::VerificationUnknown
        );
    }

    #[test]
    fn reports_only_fixed_categories_and_distinguishes_uncertain_commit() {
        assert_eq!(
            Outcome::Published.report(),
            (0, "release_publish=published\n")
        );
        assert_eq!(Outcome::Invalid.report(), (2, "release_publish=invalid\n"));
        assert_eq!(
            Outcome::StageRejected { residue: true }.report(),
            (1, "release_publish=reject stage=staging residue=present\n")
        );
        assert_eq!(
            Outcome::PublicationFailed {
                phase: PublicationPhase::UncertainCommit,
                residue: true
            }
            .report(),
            (
                3,
                "release_publish=unknown stage=publication phase=uncertain_commit residue=present\n"
            )
        );
        assert_eq!(
            Outcome::VerificationUnknown.report(),
            (3, "release_publish=unknown stage=verification\n")
        );
    }
}
