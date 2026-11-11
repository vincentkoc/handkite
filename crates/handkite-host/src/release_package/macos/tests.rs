use super::*;
use crate::release_package::tests::{CERTIFICATE, request_json};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;

struct Fixture {
    _scratch: tempfile::TempDir,
    parent: PathBuf,
    source: PathBuf,
    output: PathBuf,
    bytes: Vec<u8>,
    request: PackageRequest,
}

impl Fixture {
    fn new() -> Self {
        // Safe ancestors are part of the contract; /tmp is deliberately refused.
        let scratch = tempfile::Builder::new()
            .prefix(".package-fixture-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        chmod(scratch.path(), 0o700);
        let parent = fs::canonicalize(scratch.path()).unwrap();
        let source = parent.join("build");
        fs::create_dir(&source).unwrap();
        chmod(&source, 0o755);
        let mut bytes = vec![0; 64];
        bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        bytes[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&2_u32.to_le_bytes());
        let digest = hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
        let mut json = request_json();
        for (index, name) in NAMES.into_iter().enumerate() {
            let path = source.join(name.filename());
            fs::write(&path, &bytes).unwrap();
            chmod(&path, 0o755);
            json["candidates"][index]["sha256"] = digest.clone().into();
        }
        let request =
            PackageRequest::parse(&serde_json::to_vec(&json).unwrap(), CERTIFICATE).unwrap();
        let output = parent.join("package");
        Self {
            _scratch: scratch,
            parent,
            source,
            output,
            bytes,
            request,
        }
    }

    fn run(&self, fake: TestRun<'_>) -> Result<PackagedRelease, PackageFailure> {
        package(&self.source, &self.output, &self.request, Some(fake))
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn success(command: &mut Command, _: Duration) -> Result<ExitStatus, RunError> {
    assert_eq!(command.get_program(), OsStr::new("/usr/bin/codesign"));
    let args = command.get_args().collect::<Vec<_>>();
    if args[0] == "--force" {
        let path = Path::new(args.last().unwrap());
        let mut bytes = fs::read(path).unwrap();
        bytes.extend_from_slice(b"synthetic signature");
        let replacement = path.with_extension("fixture");
        fs::write(&replacement, bytes).unwrap();
        chmod(&replacement, 0o700);
        fs::rename(replacement, path).unwrap();
    }
    Ok(ExitStatus::from_raw(0))
}

fn failure(result: Result<PackagedRelease, PackageFailure>) -> PackageFailure {
    result.err().expect("package must fail")
}

#[test]
fn produces_only_final_signed_inventory_and_preserves_sources() {
    let fixture = Fixture::new();
    fs::write(fixture.source.join("handkited.d"), b"build metadata").unwrap();
    let mut calls = Vec::new();
    let packaged = fixture
        .run(&mut |command, timeout| {
            let args = command
                .get_args()
                .map(|arg| arg.to_os_string())
                .collect::<Vec<_>>();
            if args[0] == "--force" {
                assert_eq!(timeout, SIGN_TIMEOUT);
                let name = NAMES[calls.iter().filter(|signing| **signing).count()];
                let expected: Vec<std::ffi::OsString> = [
                    "--force",
                    "--sign",
                    CERTIFICATE,
                    "--identifier",
                    name.code_identifier(),
                    "--timestamp=none",
                    "--",
                ]
                .map(Into::into)
                .into();
                assert_eq!(args[..7], expected);
                assert_eq!(
                    Path::new(args.last().unwrap()),
                    fixture.output.join("artifacts").join(name.filename())
                );
                calls.push(true);
            } else {
                assert_eq!(timeout, Duration::from_secs(10));
                assert_eq!(args[0], "--verify");
                assert!(args[4].to_str().unwrap().contains(CERTIFICATE));
                calls.push(false);
            }
            assert_eq!(command.get_envs().count(), 0);
            assert!(format!("{command:?}").starts_with("env -i "));
            success(command, timeout)
        })
        .unwrap();
    assert_eq!(
        calls,
        [true, false, true, false, true, false, false, false, false]
    );
    let manifest = fs::read(fixture.output.join("manifest.json")).unwrap();
    let parsed = ReleaseManifest::parse(&manifest).unwrap();
    assert_eq!(parsed.canonical_json().unwrap(), manifest);
    assert_eq!(
        &parsed.canonical_sha256().unwrap(),
        packaged.manifest_sha256()
    );
    assert_eq!(fs::read_dir(&fixture.output).unwrap().count(), 2);
    assert_eq!(
        fs::read_dir(fixture.output.join("artifacts"))
            .unwrap()
            .count(),
        3
    );
    for artifact in parsed.artifacts() {
        assert_ne!(
            artifact.sha256,
            candidate(&fixture.request, artifact.name).unwrap().sha256
        );
        assert_eq!(
            fs::read(fixture.source.join(artifact.name.filename())).unwrap(),
            fixture.bytes
        );
        let path = fixture
            .output
            .join("artifacts")
            .join(artifact.name.filename());
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o500);
    }
}

#[test]
fn hash_or_macho_mismatch_rejects_before_creating_output() {
    for macho in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.source.join("handkited");
        if macho {
            let mut bytes = fixture.bytes.clone();
            bytes[4] = 7;
            fs::write(path, bytes).unwrap();
        } else {
            fs::write(path, b"changed").unwrap();
        }
        let error = failure(fixture.run(&mut |_, _| panic!("signer called")));
        assert_eq!(error.phase(), PackagePhase::Admission);
        assert!(!error.residue_present());
        assert!(!fixture.output.exists());
    }
}

#[test]
fn existing_output_and_unsafe_parent_are_never_modified() {
    for kind in 0..4 {
        let fixture = Fixture::new();
        match kind {
            0 => {
                fs::create_dir(&fixture.output).unwrap();
            }
            1 => {
                fs::write(&fixture.output, b"preserve").unwrap();
            }
            2 => {
                symlink(&fixture.source, &fixture.output).unwrap();
            }
            _ => chmod(&fixture.parent, 0o755),
        }
        let error = failure(fixture.run(&mut |_, _| panic!("signer called")));
        assert_eq!(error.phase(), PackagePhase::Admission);
        assert!(!error.residue_present());
        if kind == 1 {
            assert_eq!(fs::read(&fixture.output).unwrap(), b"preserve");
        }
        if kind == 3 {
            chmod(&fixture.parent, 0o700);
        }
    }
}

#[test]
fn source_links_bad_permissions_and_acls_reject_before_signing() {
    for kind in 0..7 {
        let fixture = Fixture::new();
        let path = fixture.source.join("handkited");
        match kind {
            0 => chmod(&path, 0o777),
            1 => chmod(&path, 0o600),
            2 => {
                fs::hard_link(&path, fixture.parent.join("alias")).unwrap();
            }
            3 => {
                let moved = fixture.parent.join("moved");
                fs::rename(&path, &moved).unwrap();
                symlink(moved, path).unwrap();
            }
            4 => {
                assert!(
                    Command::new("/bin/chmod")
                        .args(["+a", "everyone allow read"])
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            5 => chmod(&fixture.source, 0o777),
            _ => {
                let moved = fixture.parent.join("moved");
                fs::rename(&fixture.source, &moved).unwrap();
                symlink(moved, &fixture.source).unwrap();
            }
        }
        let error = failure(fixture.run(&mut |_, _| panic!("signer called")));
        assert_eq!(error.phase(), PackagePhase::Admission);
        assert!(!error.residue_present());
    }
}

#[test]
fn tool_failure_and_timeout_preserve_residue_without_retry() {
    for kind in 0..3 {
        let fixture = Fixture::new();
        let mut calls = 0;
        let error = failure(fixture.run(&mut |_, _| {
            calls += 1;
            match kind {
                0 => Ok(ExitStatus::from_raw(1 << 8)),
                1 => Err(RunError::Timeout),
                _ => Err(RunError::Unavailable),
            }
        }));
        assert_eq!(calls, 1);
        assert_eq!(error.phase(), PackagePhase::Signing);
        assert!(error.residue_present());
        assert_eq!(
            fs::read_dir(fixture.output.join("artifacts"))
                .unwrap()
                .count(),
            3
        );
        assert!(!fixture.output.join("manifest.json").exists());
    }
}

#[test]
fn rejects_source_other_artifact_stage_and_parent_replacement_during_signing() {
    for kind in 0..6 {
        let fixture = Fixture::new();
        let mut calls = 0;
        let error = failure(fixture.run(&mut |command, timeout| {
            calls += 1;
            let result = success(command, timeout);
            if calls == 1 {
                let path = match kind {
                    0 => fixture.source.join("handkited"),
                    1 => fixture.output.join("artifacts/handkitectl"),
                    2 => fixture.output.join("artifacts"),
                    3 => fixture.output.clone(),
                    4 => fixture.source.clone(),
                    _ => fixture.output.join("artifacts/extra"),
                };
                if kind == 5 {
                    fs::write(&path, b"extra").unwrap();
                } else {
                    let moved = path.with_extension("held");
                    fs::rename(&path, &moved).unwrap();
                    if kind < 2 {
                        fs::copy(moved, &path).unwrap();
                    } else {
                        fs::create_dir(&path).unwrap();
                        chmod(&path, 0o700);
                    }
                }
            }
            result
        }));
        assert_eq!(error.phase(), PackagePhase::Signing);
        assert!(error.residue_present());
        assert!(calls <= 2);
    }
}

#[test]
fn rejects_in_place_signing_and_unsafe_signed_replacements() {
    for kind in 0..5 {
        let fixture = Fixture::new();
        let error = failure(fixture.run(&mut |command, timeout| {
            let path = PathBuf::from(command.get_args().last().unwrap());
            if kind == 0 {
                fs::write(&path, &fixture.bytes).unwrap();
            } else {
                success(command, timeout)?;
                match kind {
                    1 => chmod(&path, 0o755),
                    2 => {
                        fs::hard_link(&path, fixture.parent.join("alias")).unwrap();
                    }
                    3 => {
                        fs::remove_file(&path).unwrap();
                        symlink(fixture.source.join("handkited"), path).unwrap();
                    }
                    _ => {
                        fs::write(path, b"not Mach-O").unwrap();
                    }
                }
            }
            Ok(ExitStatus::from_raw(0))
        }));
        assert_eq!(error.phase(), PackagePhase::Signing);
        assert!(error.residue_present());
    }
}

#[test]
fn signature_rejection_and_final_reverification_failure_never_succeed() {
    for fail_call in [2, 7, 9] {
        let fixture = Fixture::new();
        let mut calls = 0;
        let error = failure(fixture.run(&mut |command, timeout| {
            calls += 1;
            if calls == fail_call {
                return Ok(ExitStatus::from_raw(3 << 8));
            }
            success(command, timeout)
        }));
        assert_eq!(calls, fail_call);
        assert_eq!(
            error.phase(),
            if fail_call == 2 {
                PackagePhase::Signing
            } else {
                PackagePhase::Verification
            }
        );
        assert!(error.residue_present());
    }
}

#[test]
fn final_verification_detects_mutations_to_earlier_artifact_source_or_receipt() {
    for kind in 0..3 {
        let fixture = Fixture::new();
        let mut calls = 0;
        let error = failure(fixture.run(&mut |command, timeout| {
            calls += 1;
            let result = success(command, timeout);
            if calls == 9 {
                let path = match kind {
                    0 => fixture.output.join("artifacts/handkited"),
                    1 => fixture.source.join("handkited"),
                    _ => fixture.output.join("manifest.json"),
                };
                chmod(&path, 0o700);
                fs::write(path, b"changed").unwrap();
            }
            result
        }));
        assert_eq!(error.phase(), PackagePhase::Verification);
        assert!(error.residue_present());
    }
}

#[test]
fn replacement_must_leave_unlinked_original_bytes_unchanged() {
    let fixture = Fixture::new();
    let error = failure(fixture.run(&mut |command, timeout| {
        let path = PathBuf::from(command.get_args().last().unwrap());
        let mut bytes = fs::read(&path).unwrap();
        bytes[40] ^= 1;
        fs::write(&path, bytes).unwrap();
        success(command, timeout)
    }));
    assert_eq!(error.phase(), PackagePhase::Signing);
    assert!(error.residue_present());
}

#[test]
fn rejects_ambiguous_paths_and_source_output_overlap() {
    let fixture = Fixture::new();
    for source in [
        fixture.source.join("."),
        fixture.source.join(".."),
        PathBuf::from("relative/build"),
    ] {
        let error = failure(package(
            &source,
            &fixture.output,
            &fixture.request,
            Some(&mut |_, _| panic!("signer called")),
        ));
        assert_eq!(error.phase(), PackagePhase::Admission);
        assert!(!error.residue_present());
    }
    let error = failure(package(
        &fixture.source,
        &fixture.source.join("package"),
        &fixture.request,
        Some(&mut |_, _| panic!("signer called")),
    ));
    assert_eq!(error.phase(), PackagePhase::Admission);
    assert!(!error.residue_present());
}

#[test]
fn bounded_copy_rejects_growth_without_copying_the_extra_byte() {
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"12345").unwrap();
    let mut destination = tempfile::tempfile().unwrap();
    assert!(digest_bytes(&mut source, 4, Some(&mut destination)).is_err());
    assert!(destination.metadata().unwrap().len() <= 4);
}
