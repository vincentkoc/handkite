use super::*;
use crate::release_files::tests::{Fixture, chmod};
use crate::release_manifest::{
    ArtifactName, MAX_ARTIFACT_BYTES, ReleaseArtifact, RustIdentity, SdkIdentity,
};
use std::fs;
use std::os::unix::fs::symlink;

const CERTIFICATE: &str = "1111111111111111111111111111111111111111";

fn hash(bytes: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .unwrap()
}

fn manifest(fixture: &Fixture) -> Vec<u8> {
    ReleaseManifest::new(
        "a".repeat(40),
        RustIdentity {
            toolchain: "1.95.0".into(),
            compiler_commit: "b".repeat(40),
        },
        SdkIdentity {
            version: "26.4".into(),
            build: "25E1".into(),
        },
        NAMES.map(|name| {
            let bytes = fs::read(fixture.file(name)).unwrap();
            let digest = hash(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            ReleaseArtifact::new(name, bytes.len() as u64, digest)
        }),
    )
    .unwrap()
    .canonical_json()
    .unwrap()
}

fn run(
    fixture: &Fixture,
    bytes: &[u8],
    mut check: impl FnMut(ArtifactName, &Path) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    verify(
        &fixture.stage,
        bytes,
        &hash(bytes),
        CERTIFICATE,
        Some(&mut check),
    )
}

fn overwrite(path: &Path, bytes: &[u8]) {
    chmod(path, 0o700);
    fs::write(path, bytes).unwrap();
    chmod(path, 0o500);
}

#[test]
fn checks_exact_fixed_artifacts_in_order_without_creating_or_running_inputs() {
    let fixture = Fixture::new();
    let marker = fixture.scratch.path().join("must-not-run");
    for name in NAMES {
        let script = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
        overwrite(&fixture.file(name), script.as_bytes());
    }
    let bytes = manifest(&fixture);
    let mut checked = Vec::new();
    run(&fixture, &bytes, |name, path| {
        assert_eq!(path, fixture.file(name));
        checked.push(name);
        Ok(())
    })
    .unwrap();
    assert_eq!(checked, NAMES);
    assert!(!marker.exists());
    assert_eq!(fs::read_dir(&fixture.stage).unwrap().count(), 3);
    // The public route has no injected checker and rejects these unsigned scripts.
    assert!(
        crate::release_verify::verify(&fixture.stage, &bytes, &hash(&bytes), CERTIFICATE).is_err()
    );
    assert!(!marker.exists());
}

#[test]
fn rejects_trust_inputs_before_path_or_checker_access() {
    let fixture = Fixture::new();
    let bytes = manifest(&fixture);
    let missing = fixture.scratch.path().join("missing");
    let mut never = |_, _: &Path| -> Result<(), &'static str> { panic!("checker must not run") };
    assert_eq!(
        verify(&missing, &bytes, &[0; 32], CERTIFICATE, Some(&mut never)),
        Err("release manifest pin mismatch")
    );
    assert_eq!(
        verify(&missing, b"{}", &hash(b"{}"), CERTIFICATE, Some(&mut never)),
        Err("invalid release manifest JSON")
    );
    assert_eq!(
        verify(&missing, &bytes, &hash(&bytes), "bad", Some(&mut never)),
        Err("invalid signer certificate SHA1")
    );
    let mut spaced = bytes.clone();
    spaced.push(b'\n');
    assert_eq!(
        verify(
            &missing,
            &spaced,
            &hash(&bytes),
            CERTIFICATE,
            Some(&mut never)
        ),
        Err("release manifest pin mismatch")
    );
    assert_eq!(
        verify(
            &missing,
            &spaced,
            &hash(&spaced),
            CERTIFICATE,
            Some(&mut never)
        ),
        Err("release manifest is not canonical")
    );
    assert!(!missing.exists());
}

#[test]
fn rejects_bad_size_digest_and_invalid_manifest_before_signer() {
    let fixture = Fixture::new();
    let bytes = manifest(&fixture);
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for bad_size in [0, MAX_ARTIFACT_BYTES + 1] {
        value["artifacts"][0]["size"] = bad_size.into();
        let changed = serde_json::to_vec(&value).unwrap();
        assert!(run(&fixture, &changed, |_, _| panic!("checker must not run")).is_err());
    }
    for mode in [0, 1, 2] {
        let mut parsed = ReleaseManifest::parse(&bytes).unwrap();
        // Construct a valid canonical manifest with a wrong size or digest.
        let mut records: [ReleaseArtifact; 3] =
            serde_json::from_value(value["artifacts"].clone()).unwrap();
        for item in &mut records {
            item.size = 24;
        }
        if mode == 0 {
            records[0].size = 23;
        }
        if mode == 1 {
            records[0].size = 25;
        }
        if mode == 2 {
            records[0].sha256 = "0".repeat(64);
        }
        parsed = ReleaseManifest::new(
            parsed.source_sha().into(),
            RustIdentity {
                toolchain: parsed.rust().toolchain.clone(),
                compiler_commit: parsed.rust().compiler_commit.clone(),
            },
            SdkIdentity {
                version: parsed.sdk().version.clone(),
                build: parsed.sdk().build.clone(),
            },
            records,
        )
        .unwrap();
        let changed = parsed.canonical_json().unwrap();
        assert_eq!(
            run(&fixture, &changed, |_, _| panic!("checker must not run")),
            Err("release artifact bytes mismatch")
        );
    }
}

#[test]
fn signer_failure_stops_at_the_failed_artifact() {
    for failure in 0..3 {
        let fixture = Fixture::new();
        let bytes = manifest(&fixture);
        let mut calls = 0;
        assert_eq!(
            run(&fixture, &bytes, |_, _| {
                let index = calls;
                calls += 1;
                if index == failure {
                    Err("fixture signer failure")
                } else {
                    Ok(())
                }
            }),
            Err("fixture signer failure")
        );
        assert_eq!(calls, failure + 1);
    }
}

#[test]
fn rejects_each_binary_mutated_during_each_signer_including_first_during_third() {
    for signer in NAMES {
        for victim in NAMES {
            let fixture = Fixture::new();
            let bytes = manifest(&fixture);
            assert!(
                run(&fixture, &bytes, |name, _| {
                    if name == signer {
                        overwrite(&fixture.file(victim), b"modified executable data");
                    }
                    Ok(())
                })
                .is_err()
            );
        }
    }
}

#[test]
fn rejects_path_inventory_mode_and_acl_changes_during_signer() {
    for mutation in 0..6 {
        let fixture = Fixture::new();
        let bytes = manifest(&fixture);
        assert!(
            run(&fixture, &bytes, |name, path| {
                if name != ArtifactName::Mcp {
                    return Ok(());
                }
                match mutation {
                    0 => {
                        fs::write(fixture.stage.join("extra"), b"extra").unwrap();
                    }
                    1 => {
                        fs::remove_file(fixture.file(ArtifactName::Daemon)).unwrap();
                    }
                    2 => {
                        chmod(&fixture.file(ArtifactName::Daemon), 0o700);
                    }
                    3 => {
                        let target = fixture.file(ArtifactName::Daemon);
                        fs::remove_file(&target).unwrap();
                        symlink(path, target).unwrap();
                    }
                    4 => {
                        let moved = fixture.scratch.path().join("replaced");
                        fs::rename(&fixture.stage, &moved).unwrap();
                        fs::create_dir(&fixture.stage).unwrap();
                        chmod(&fixture.stage, 0o700);
                    }
                    _ => {
                        assert!(
                            std::process::Command::new("/bin/chmod")
                                .args(["+a", "everyone allow read"])
                                .arg(fixture.file(ArtifactName::Daemon))
                                .status()
                                .unwrap()
                                .success()
                        );
                    }
                }
                Ok(())
            })
            .is_err()
        );
    }
}

#[test]
fn hash_reads_are_bounded_and_second_pass_restarts_at_zero() {
    let fixture = Fixture::new();
    let bytes = manifest(&fixture);
    let manifest = ReleaseManifest::parse(&bytes).unwrap();
    let mut held = AnchoredRelease::open(&fixture.stage).unwrap();
    hash_all(&mut held, &manifest).unwrap();
    hash_all(&mut held, &manifest).unwrap();
    let file = &mut held.artifacts[0];
    overwrite(&fixture.file(ArtifactName::Daemon), &[b'x'; 512]);
    assert!(hash_file(file, &manifest.artifacts()[0]).is_err());
    assert_eq!(file.file.stream_position().unwrap(), 25);
    overwrite(&fixture.file(ArtifactName::Daemon), b"short");
    assert!(hash_file(file, &manifest.artifacts()[0]).is_err());
}
