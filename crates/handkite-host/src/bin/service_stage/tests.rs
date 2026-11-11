use super::*;
use handkite_host::release_manifest::{ReleaseArtifact, RustIdentity, SdkIdentity};
use std::cell::Cell;
use std::io::{Read, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use tempfile::NamedTempFile;

fn manifest() -> Vec<u8> {
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
        [ArtifactName::Daemon, ArtifactName::Cli, ArtifactName::Mcp]
            .map(|name| ReleaseArtifact::new(name, 1, "c".repeat(64))),
    )
    .unwrap()
    .canonical_json()
    .unwrap()
}

fn private(bytes: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file
}

fn envelope(manifest: &[u8]) -> Vec<u8> {
    let hash = ring::digest::digest(&ring::digest::SHA256, manifest);
    let mut bytes = format!(
        "{}\n{}\n",
        hash.as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "1".repeat(40)
    )
    .into_bytes();
    bytes.extend_from_slice(manifest);
    bytes
}

fn expected() -> NamedTempFile {
    private(br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#)
}

#[test]
fn exact_arguments_and_independent_private_inputs_dispatch_once() {
    assert!(arguments(
        ["--binding-fd", "3"].map(OsString::from).into_iter()
    ));
    for args in [
        vec![],
        vec!["--binding-fd"],
        vec!["--binding-fd", "0"],
        vec!["--binding-fd", "03"],
        vec!["--binding-fd", "3", "--force"],
        vec!["--stage", "/fixture"],
    ] {
        assert!(!arguments(args.into_iter().map(OsString::from)));
    }
    let bytes = manifest();
    let mut input = private(&envelope(&bytes));
    let mut expected = expected();
    let calls = Cell::new(0);
    let outcome = run(
        input.as_file_mut(),
        expected.as_file_mut(),
        |pins, binding| {
            calls.set(calls.get() + 1);
            assert_eq!(pins.manifest, bytes);
            assert_eq!(binding.key.as_str(), "phone");
            assert_eq!(binding.usb_id.as_str(), "test-device");
            Outcome::Staged
        },
    );
    assert_eq!(outcome, Outcome::Staged);
    assert_eq!(calls.get(), 1);
}

#[test]
fn invalid_pins_manifest_or_binding_never_reaches_setup() {
    for variant in 0..11 {
        let mut bytes = envelope(&manifest());
        let mut binding =
            br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#.to_vec();
        match variant {
            0 => bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' },
            1 => bytes[65] = b'A',
            2 => bytes = envelope(b"{}"),
            3 => {
                let mut value = manifest();
                value.push(b'\n');
                bytes = envelope(&value);
            }
            4 => {
                binding = br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","extra":true}"#.to_vec()
            }
            5 => {
                binding = br#"{"schema_version":1,"device_key":"phone","device_key":"phone","usb_id":"test-device"}"#
                    .to_vec()
            }
            6 => binding = br#"{"schema_version":1,"device_key":"phone"}"#.to_vec(),
            7 => binding = br#"{"schema_version":1,"device_key":"../phone","usb_id":"test-device"}"#.to_vec(),
            8 => binding = br#"{"schema_version":1,"device_key":"phone","usb_id":"bad identity"}"#.to_vec(),
            9 => binding = vec![b' '; 4097],
            _ => binding = br#"[1,"phone","test-device"]"#.to_vec(),
        }
        let mut input = private(&bytes);
        let mut expected = private(&binding);
        assert_eq!(
            run(input.as_file_mut(), expected.as_file_mut(), |_, _| panic!(
                "invalid admission reached setup"
            )),
            Outcome::Invalid
        );
    }
}

#[test]
fn private_inputs_reject_modes_links_offsets_shared_inode_and_acl() {
    for variant in 0..8 {
        let mut input = private(&envelope(&manifest()));
        let mut expected = expected();
        let link = tempfile::tempdir().unwrap();
        match variant {
            0 => std::fs::set_permissions(input.path(), std::fs::Permissions::from_mode(0o644))
                .unwrap(),
            1 => std::fs::set_permissions(expected.path(), std::fs::Permissions::from_mode(0o400))
                .unwrap(),
            2 => std::fs::hard_link(input.path(), link.path().join("linked")).unwrap(),
            3 => std::fs::hard_link(expected.path(), link.path().join("linked")).unwrap(),
            4 => {
                input.seek(SeekFrom::Start(1)).unwrap();
            }
            5 => {
                expected.seek(SeekFrom::Start(1)).unwrap();
            }
            6 => {
                let mut clone = input.as_file().try_clone().unwrap();
                assert_eq!(
                    run(input.as_file_mut(), &mut clone, |_, _| panic!(
                        "same inode admitted"
                    )),
                    Outcome::Invalid
                );
                continue;
            }
            _ => assert!(
                std::process::Command::new("/bin/chmod")
                    .args(["+a", "everyone allow read"])
                    .arg(expected.path())
                    .status()
                    .unwrap()
                    .success()
            ),
        }
        assert_eq!(
            run(input.as_file_mut(), expected.as_file_mut(), |_, _| panic!(
                "unsafe input reached setup"
            )),
            Outcome::Invalid
        );
    }
}

#[test]
fn changed_input_during_read_is_rejected_without_accepting_new_baseline() {
    let mut input = private(b"original");
    let path = input.path().to_path_buf();
    assert!(
        protected_read(input.as_file_mut(), 4096, |file| {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).ok()?;
            std::fs::write(&path, b"modified").unwrap();
            Some(bytes)
        })
        .is_none()
    );
}

#[test]
fn all_outcomes_are_fixed_and_never_claim_activation_or_selection() {
    assert_eq!(Outcome::Staged.report(), (0, "service_stage=staged\n"));
    for outcome in [
        Outcome::Invalid,
        Outcome::SetupRejected,
        Outcome::ConfigRejected,
        Outcome::ReleaseRejected,
        Outcome::Failed(StagePhase::BeforeRename, false),
        Outcome::Failed(StagePhase::BeforeRename, true),
        Outcome::Failed(StagePhase::AppliedRename, false),
        Outcome::Failed(StagePhase::AppliedRename, true),
        Outcome::Failed(StagePhase::UncertainRename, false),
        Outcome::Failed(StagePhase::UncertainRename, true),
    ] {
        let (code, text) = outcome.report();
        assert_ne!(code, 0);
        assert!(text.starts_with("service_stage="));
        assert!(text.ends_with('\n'));
        assert!(
            !text.contains('/')
                && !text.contains("installed")
                && !text.contains("ready")
                && !text.contains("selected")
        );
    }
}
