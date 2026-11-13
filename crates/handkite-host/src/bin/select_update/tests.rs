use super::*;
use handkite_host::release_manifest::{
    ArtifactName, ReleaseArtifact, ReleaseManifest, RustIdentity, SdkIdentity,
};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use tempfile::NamedTempFile;

fn manifest(digit: char) -> Vec<u8> {
    ReleaseManifest::new(
        digit.to_string().repeat(40),
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

fn envelope(bytes: &[u8], certificate: char) -> Vec<u8> {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut output = format!(
        "{}\n{}\n",
        digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        certificate.to_string().repeat(40)
    )
    .into_bytes();
    output.extend_from_slice(bytes);
    output
}

fn private(bytes: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.rewind().unwrap();
    file
}

fn inputs() -> [NamedTempFile; 3] {
    [
        private(&envelope(&manifest('a'), '1')),
        private(br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device"}"#),
        private(&envelope(&manifest('d'), '2')),
    ]
}

fn admit(files: &mut [NamedTempFile; 3]) -> Option<()> {
    let [destination, binding, previous] = files;
    install_input::run_update(
        destination.as_file_mut(),
        binding.as_file_mut(),
        previous.as_file_mut(),
        |_, _, _| (),
    )
}

#[test]
fn arguments_require_an_explicit_operation_and_exact_descriptors() {
    for (name, operation) in [
        ("upgrade", UpdateOperation::Upgrade),
        ("rollback", UpdateOperation::Rollback),
    ] {
        let args = [
            "--operation",
            name,
            "--binding-fd",
            "3",
            "--previous-fd",
            "4",
        ];
        assert_eq!(
            arguments(args.map(OsString::from).into_iter()),
            Some(operation)
        );
    }
    for args in [
        vec![],
        vec!["--operation", "initial-install"],
        vec![
            "--operation",
            "upgrade",
            "--binding-fd",
            "3",
            "--previous-fd",
            "04",
        ],
        vec![
            "--operation",
            "upgrade",
            "--binding-fd",
            "0",
            "--previous-fd",
            "4",
        ],
        vec![
            "--operation",
            "upgrade",
            "--binding-fd",
            "3",
            "--previous-fd",
            "4",
            "--retry",
        ],
        vec!["--root", "/fixture"],
    ] {
        assert_eq!(arguments(args.into_iter().map(OsString::from)), None);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn three_private_inputs_preserve_both_independent_release_pins() {
    let [mut destination, mut binding, mut previous] = inputs();
    let mut calls = 0;
    assert_eq!(
        install_input::run_update(
            destination.as_file_mut(),
            binding.as_file_mut(),
            previous.as_file_mut(),
            |to, from, binding| {
                calls += 1;
                assert_eq!(to.manifest, manifest('a'));
                assert_eq!(from.manifest, manifest('d'));
                assert_eq!(to.certificate, "1".repeat(40));
                assert_eq!(from.certificate, "2".repeat(40));
                assert_eq!(binding.key.as_str(), "phone");
                assert_eq!(binding.usb_id.as_str(), "test-device");
            }
        ),
        Some(())
    );
    assert_eq!(calls, 1);
}

#[cfg(target_os = "macos")]
#[test]
fn every_input_rejects_public_mode_offset_hardlink_and_acl() {
    for index in 0..3 {
        for variant in 0..4 {
            let mut files = inputs();
            let directory = tempfile::tempdir().unwrap();
            match variant {
                0 => files[index]
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o644))
                    .unwrap(),
                1 => {
                    files[index].seek(SeekFrom::Start(1)).unwrap();
                }
                2 => {
                    std::fs::hard_link(files[index].path(), directory.path().join("alias")).unwrap()
                }
                _ => assert!(
                    std::process::Command::new("/bin/chmod")
                        .args(["+a", "everyone allow read"])
                        .arg(files[index].path())
                        .status()
                        .unwrap()
                        .success()
                ),
            }
            assert_eq!(admit(&mut files), None, "input {index}, variant {variant}");
        }
    }
}

#[test]
fn aliased_input_descriptors_refuse_before_reading() {
    for (left, right) in [(0, 1), (0, 2), (1, 2)] {
        let files = inputs();
        let mut descriptors: [File; 3] =
            std::array::from_fn(|index| files[index].as_file().try_clone().unwrap());
        descriptors[right] = descriptors[left].try_clone().unwrap();
        let [destination, binding, previous] = &mut descriptors;
        assert_eq!(
            install_input::run_update(destination, binding, previous, |_, _, _| panic!(
                "alias admitted"
            )),
            None
        );
        assert!(
            descriptors
                .iter_mut()
                .all(|file| file.stream_position().unwrap() == 0)
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn malformed_previous_same_release_or_binding_never_reaches_setup() {
    for variant in 0..6 {
        let mut files = inputs();
        match variant {
            0 => files[2] = private(b"invalid"),
            1 => {
                let mut bytes = envelope(&manifest('d'), '2');
                bytes[0] = b'z';
                files[2] = private(&bytes);
            }
            2 => files[2] = private(&envelope(&manifest('a'), '2')),
            3 => files[1] = private(
                br#"{"schema_version":1,"device_key":"phone","usb_id":"test-device","extra":true}"#,
            ),
            4 => {
                let mut bytes = manifest('d');
                bytes.push(b'\n');
                files[2] = private(&envelope(&bytes, '2'));
            }
            _ => files[2] = private(&vec![b'a'; 17000]),
        }
        let [destination, binding, previous] = &mut files;
        assert_eq!(
            install_input::run_update(
                destination.as_file_mut(),
                binding.as_file_mut(),
                previous.as_file_mut(),
                |_, _, _| panic!("invalid input admitted")
            ),
            None
        );
    }
}

#[test]
fn open_pipes_refuse_without_reading() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    for index in 0..3 {
        let files = inputs();
        let mut descriptors: [File; 3] =
            std::array::from_fn(|index| files[index].as_file().try_clone().unwrap());
        let (stream, _writer) = UnixStream::pair().unwrap();
        descriptors[index] = File::from(OwnedFd::from(stream));
        let [destination, binding, previous] = &mut descriptors;
        assert_eq!(
            install_input::run_update(destination, binding, previous, |_, _, _| panic!(
                "pipe admitted"
            )),
            None
        );
    }
}

#[test]
fn failure_output_distinguishes_phase_from_selection_application() {
    for (stage, phase, applied, expected, code) in [
        (
            SelectionStage::Preparation,
            SelectionPhase::Before,
            false,
            "reject stage=preparation phase=before applied=no",
            1,
        ),
        (
            SelectionStage::Commit,
            SelectionPhase::Before,
            false,
            "reject stage=commit phase=before applied=no",
            1,
        ),
        (
            SelectionStage::Commit,
            SelectionPhase::Applied,
            true,
            "unknown stage=commit phase=applied applied=yes",
            3,
        ),
        (
            SelectionStage::Commit,
            SelectionPhase::Uncertain,
            true,
            "unknown stage=commit phase=uncertain applied=unknown",
            3,
        ),
        (
            SelectionStage::Retirement,
            SelectionPhase::Before,
            true,
            "unknown stage=retirement phase=before applied=yes",
            3,
        ),
        (
            SelectionStage::Retirement,
            SelectionPhase::Applied,
            true,
            "unknown stage=retirement phase=applied applied=yes",
            3,
        ),
        (
            SelectionStage::Retirement,
            SelectionPhase::Uncertain,
            true,
            "unknown stage=retirement phase=uncertain applied=yes",
            3,
        ),
        (
            SelectionStage::Verification,
            SelectionPhase::Applied,
            true,
            "unknown stage=verification phase=applied applied=yes",
            3,
        ),
    ] {
        let mut output = Vec::new();
        assert_eq!(
            report(
                Outcome::Failed {
                    stage,
                    phase,
                    may_be_applied: applied,
                    residue: true
                },
                &mut output
            ),
            code
        );
        assert_eq!(
            output,
            format!("update_select={expected} residue=present\n").into_bytes()
        );
    }
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(report(Outcome::Selected, &mut Broken), 3);
}
