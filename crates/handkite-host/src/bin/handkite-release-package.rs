//! Explicit package production from independently supplied candidate and signer inputs.

use handkite_host::release_package::{self, MAX_REQUEST_BYTES, PackagePhase, PackageRequest};
use handkite_host::release_verify::check_private_input_acl;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HEADER_BYTES: usize = 41;
const MAX_INPUT_BYTES: usize = HEADER_BYTES + MAX_REQUEST_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Inventory,
    Invalid,
    Rejected { phase: PackagePhase, residue: bool },
}

impl Outcome {
    fn report(self) -> (u8, String) {
        match self {
            Self::Inventory => (0, "release_package=inventory\n".into()),
            Self::Invalid => (2, "release_package=invalid\n".into()),
            Self::Rejected { phase, residue } => {
                let phase = match phase {
                    PackagePhase::Admission => "admission",
                    PackagePhase::Copy => "copy",
                    PackagePhase::Signing => "signing",
                    PackagePhase::Inventory => "inventory",
                    PackagePhase::Verification => "verification",
                };
                let residue = if residue { "present" } else { "absent" };
                (
                    1,
                    format!("release_package=reject phase={phase} residue={residue}\n"),
                )
            }
        }
    }
}

fn main() -> ExitCode {
    let outcome = match io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => run(std::env::args_os().skip(1), &mut File::from(fd), produce),
        Err(_) => Outcome::Invalid,
    };
    let (code, message) = outcome.report();
    if io::stdout().lock().write_all(message.as_bytes()).is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}

fn produce(source: &Path, output: &Path, request: &PackageRequest) -> Outcome {
    match release_package::package(source, output, request) {
        Ok(_) => Outcome::Inventory,
        Err(error) => Outcome::Rejected {
            phase: error.phase(),
            residue: error.residue_present(),
        },
    }
}

fn run(
    args: impl Iterator<Item = OsString>,
    input: &mut File,
    package: impl FnOnce(&Path, &Path, &PackageRequest) -> Outcome,
) -> Outcome {
    let Some((source, output)) = arguments(args) else {
        return Outcome::Invalid;
    };
    let Some(request) = read_request(input) else {
        return Outcome::Invalid;
    };
    package(&source, &output, &request)
}

fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<(PathBuf, PathBuf)> {
    if args.next()?.as_os_str() != "--source" {
        return None;
    }
    let source = PathBuf::from(args.next()?);
    if args.next()?.as_os_str() != "--output" {
        return None;
    }
    let output = PathBuf::from(args.next()?);
    (args.next().is_none() && source.is_absolute() && output.is_absolute())
        .then_some((source, output))
}

fn read_request(input: &mut File) -> Option<PackageRequest> {
    let before = input.metadata().ok()?;
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 1
        || before.len() <= HEADER_BYTES as u64
        || before.len() > MAX_INPUT_BYTES as u64
    {
        return None;
    }
    check_private_input_acl(input).ok()?;
    let mut bytes = Vec::new();
    (&mut *input)
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    check_private_input_acl(input).ok()?;
    if fingerprint(&before) != fingerprint(&input.metadata().ok()?)
        || bytes.len() as u64 != before.len()
        || bytes.get(40) != Some(&b'\n')
    {
        return None;
    }
    let certificate = std::str::from_utf8(&bytes[..40]).ok()?;
    PackageRequest::parse(&bytes[HEADER_BYTES..], certificate).ok()
}

fn fingerprint(meta: &Metadata) -> (u64, u64, u32, u32, u32, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.uid(),
        meta.gid(),
        meta.mode(),
        meta.nlink(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    const CERTIFICATE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn input() -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{CERTIFICATE}").unwrap();
        let candidates = ["handkited", "handkitectl", "handkite-mcp"].map(|name| {
            serde_json::json!({
                "name": name, "size": 64, "sha256": "c".repeat(64)
            })
        });
        let json = serde_json::json!({
            "source_sha": "a".repeat(40), "target": "aarch64-apple-darwin",
            "rust": { "toolchain": "1.95.0", "compiler_commit": "b".repeat(40) },
            "sdk": { "version": "26.0", "build": "25A1" }, "candidates": candidates
        });
        file.write_all(&serde_json::to_vec(&json).unwrap()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    fn args() -> impl Iterator<Item = OsString> {
        ["--source", "/private/source", "--output", "/private/output"]
            .map(Into::into)
            .into_iter()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn explicit_inputs_reach_producer_once_without_exposing_pin_in_output() {
        let mut input = input();
        let mut calls = 0;
        let outcome = run(args(), input.as_file_mut(), |source, output, request| {
            calls += 1;
            assert_eq!(source, Path::new("/private/source"));
            assert_eq!(output, Path::new("/private/output"));
            assert_eq!(request.certificate_sha1(), CERTIFICATE);
            assert_eq!(request.source_sha(), "a".repeat(40));
            Outcome::Inventory
        });
        assert_eq!(calls, 1);
        assert_eq!(outcome.report(), (0, "release_package=inventory\n".into()));
    }

    #[test]
    fn malformed_arguments_and_input_never_enter_producer() {
        for arguments in [
            vec![],
            vec!["--source", "relative", "--output", "/output"],
            vec!["--source", "/source", "--output", "/output", "--retry"],
            vec!["--output", "/output", "--source", "/source"],
        ] {
            let mut input = input();
            assert_eq!(
                run(
                    arguments.into_iter().map(Into::into),
                    input.as_file_mut(),
                    |_, _, _| panic!("producer called")
                ),
                Outcome::Invalid
            );
        }
        let mut bad = NamedTempFile::new().unwrap();
        bad.write_all(b"private untrusted content").unwrap();
        bad.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            run(args(), bad.as_file_mut(), |_, _, _| panic!(
                "producer called"
            )),
            Outcome::Invalid
        );
    }

    #[test]
    fn private_input_rejects_modes_links_oversize_and_nonzero_offset() {
        for kind in 0..4 {
            let mut input = input();
            let scratch = tempfile::tempdir().unwrap();
            match kind {
                0 => input
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o644))
                    .unwrap(),
                1 => std::fs::hard_link(input.path(), scratch.path().join("alias")).unwrap(),
                2 => input.as_file().set_len(MAX_INPUT_BYTES as u64 + 1).unwrap(),
                _ => {
                    input.seek(SeekFrom::Start(1)).unwrap();
                }
            }
            assert_eq!(
                run(args(), input.as_file_mut(), |_, _, _| panic!(
                    "producer called"
                )),
                Outcome::Invalid
            );
        }
    }

    #[test]
    fn output_contains_only_fixed_phase_and_residue_categories() {
        assert_eq!(
            Outcome::Invalid.report(),
            (2, "release_package=invalid\n".into())
        );
        for phase in [
            PackagePhase::Admission,
            PackagePhase::Copy,
            PackagePhase::Signing,
            PackagePhase::Inventory,
            PackagePhase::Verification,
        ] {
            for residue in [false, true] {
                let (code, message) = Outcome::Rejected { phase, residue }.report();
                assert_eq!(code, 1);
                assert!(message.starts_with("release_package=reject phase="));
                assert!(message.ends_with(if residue {
                    "residue=present\n"
                } else {
                    "residue=absent\n"
                }));
                assert!(!message.contains(CERTIFICATE));
                assert!(!message.contains('/'));
            }
        }
    }
}
