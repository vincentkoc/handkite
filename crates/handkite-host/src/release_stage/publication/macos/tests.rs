use super::*;
use crate::installer_lock::InstallerLock;
use crate::release_files::NAMES;
use crate::release_files::tests::{Fixture as SourceFixture, chmod};
use crate::release_manifest::{ReleaseArtifact, ReleaseManifest, RustIdentity, SdkIdentity};
use crate::service_layout::{
    ServiceLayout,
    tests::{fixture_in, layout_at},
};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

mod retained;
mod selection_intent;

const CERTIFICATE: &str = "1111111111111111111111111111111111111111";
const NONCE: [u8; 16] = [0x24; 16];
const TEMPORARY: &str = ".receipt-24242424242424242424242424242424";

fn hash(bytes: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .unwrap()
}

fn manifest(source: &Path) -> Vec<u8> {
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
            let bytes = fs::read(source.join(name.filename())).unwrap();
            ReleaseArtifact::new(
                name,
                bytes.len() as u64,
                hash(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        }),
    )
    .unwrap()
    .canonical_json()
    .unwrap()
}

fn staged<'lock>(owner: &'lock InstallerLock, source: &Path, bytes: &[u8]) -> StagedRelease<'lock> {
    crate::release_stage::macos::stage(
        owner,
        source,
        bytes,
        &hash(bytes),
        CERTIFICATE,
        &mut crate::release_stage::macos::Hooks::with_check(&mut |_, _| Ok(())),
    )
    .unwrap()
}

struct Fixture {
    _home: tempfile::TempDir,
    source: SourceFixture,
    owner: InstallerLock,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let (home, layout) = fixture_in(&std::env::current_dir().unwrap());
        for path in [
            layout.config_file(),
            layout.state_directory().join("UNLOCK-DISABLED"),
            layout.state_directory().join("state.json"),
        ] {
            fs::write(&path, b"preserved-private-state").unwrap();
            chmod(&path, 0o600);
        }
        symlink(
            String::from_utf8(encode_target(&[0x99; 32]).to_vec()).unwrap(),
            layout.current_release_selection(),
        )
        .unwrap();
        let source = SourceFixture::new();
        let bytes = manifest(&source.stage);
        Self {
            _home: home,
            source,
            owner: InstallerLock::acquire(&layout).unwrap(),
            bytes,
        }
    }
    fn layout(&self) -> &ServiceLayout {
        self.owner.layout()
    }
    fn stage(&self) -> StagedRelease<'_> {
        staged(&self.owner, &self.source.stage, &self.bytes)
    }
    fn name(&self) -> String {
        String::from_utf8(encode_target(&hash(&self.bytes)).to_vec()).unwrap()
    }
    fn receipt_name(&self) -> String {
        format!("{}.manifest.json", self.name())
    }
    fn directory(&self) -> std::path::PathBuf {
        self.layout().releases_directory().join(self.name())
    }
    fn receipt(&self) -> std::path::PathBuf {
        self.layout().releases_directory().join(self.receipt_name())
    }
    fn protected(&self) -> Vec<(std::path::PathBuf, Fingerprint)> {
        [
            self.layout().config_file(),
            self.layout().state_directory().join("UNLOCK-DISABLED"),
            self.layout().state_directory().join("state.json"),
            self.layout().current_release_selection(),
        ]
        .into_iter()
        .map(|path| {
            let fingerprint = Fingerprint::from(fs::symlink_metadata(&path).unwrap());
            (path, fingerprint)
        })
        .collect()
    }
    fn unchanged(&self, before: Vec<(std::path::PathBuf, Fingerprint)>) {
        for (path, fingerprint) in before {
            assert_eq!(
                fingerprint,
                Fingerprint::from(fs::symlink_metadata(path).unwrap())
            );
        }
    }
}

fn run<'lock>(
    value: StagedRelease<'lock>,
    mut fault: impl FnMut(Point, &Path) -> Result<(), &'static str>,
) -> Result<PublishedRelease<'lock>, PublicationFailure> {
    publish(
        value,
        &mut Hooks {
            check: Some(&mut |_, _| Ok(())),
            fault: Some(&mut fault),
            nonce: Some(NONCE),
            ..Hooks::production()
        },
    )
}

fn acl(path: &Path) {
    assert!(
        Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn publishes_receipt_first_and_retains_original_inodes_without_selection_changes() {
    let fixture = Fixture::new();
    let before = fixture.protected();
    let value = fixture.stage();
    let old_name = value.directory_name().to_owned();
    let original = value._destination.private_directory_identities().unwrap();
    let artifacts: Vec<_> = value
        ._destination
        .artifacts
        .iter()
        .map(|file| file.fingerprint.clone())
        .collect();
    let mut checked = Vec::new();
    let mut check = |name, path: &Path| {
        checked.push((name, path.to_path_buf()));
        Ok(())
    };
    let mut fault = |point, _: &Path| {
        if point == Point::BeforeStageRename {
            assert!(fixture.receipt().is_file());
            assert!(!fixture.directory().exists());
        }
        Ok(())
    };
    let mut hooks = Hooks {
        check: Some(&mut check),
        fault: Some(&mut fault),
        nonce: Some(NONCE),
        ..Hooks::production()
    };
    let published = publish(value, &mut hooks).unwrap();
    assert_eq!(hooks.renames, [Rename::Receipt, Rename::Stage]);
    assert_eq!(published.directory_name(), fixture.name());
    assert_eq!(published.receipt_name(), fixture.receipt_name());
    assert_eq!(
        published
            .staged
            ._destination
            .private_directory_identities()
            .unwrap(),
        original
    );
    for (file, before) in published
        .staged
        ._destination
        .artifacts
        .iter()
        .zip(artifacts)
    {
        assert_eq!(file.fingerprint, before);
    }
    assert_eq!(checked.len(), 6);
    for (index, (name, path)) in checked.into_iter().enumerate() {
        assert_eq!(name, NAMES[index % 3]);
        let directory = if index < 3 {
            fixture.layout().releases_directory().join(&old_name)
        } else {
            fixture.directory()
        };
        assert_eq!(path, directory.join(name.filename()));
    }
    assert!(
        !fixture
            .layout()
            .releases_directory()
            .join(old_name)
            .exists()
    );
    assert!(
        !fixture
            .layout()
            .releases_directory()
            .join(TEMPORARY)
            .exists()
    );
    assert_eq!(fs::read(fixture.receipt()).unwrap(), fixture.bytes);
    assert_eq!(
        fs::metadata(fixture.receipt()).unwrap().mode() & 0o7777,
        0o400
    );
    assert_eq!(
        unsafe { libc::fcntl(published._receipt.file.as_raw_fd(), libc::F_GETFL) }
            & libc::O_ACCMODE,
        libc::O_RDONLY
    );
    assert_eq!(fs::read_dir(fixture.directory()).unwrap().count(), 3);
    fixture.unchanged(before);
    drop(published);
    assert!(fixture.directory().is_dir());
    assert!(fixture.receipt().is_file());
}

#[test]
fn public_publisher_rechecks_real_signer_before_receipt_creation() {
    let fixture = Fixture::new();
    let value = fixture.stage();
    let old = value.directory_name().to_owned();
    let error = crate::release_stage::publish(value).err().unwrap();
    assert_eq!(
        error.reason(),
        "artifact signature does not satisfy signer policy"
    );
    assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
    assert_eq!(error.directory_residues(), [old]);
    assert!(error.receipt_residues().is_empty());
    assert!(!fixture.receipt().exists());
    assert!(!fixture.directory().exists());
}

#[test]
fn selected_digest_is_refused_even_when_current_target_is_dangling() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.layout().current_release_selection()).unwrap();
    symlink(fixture.name(), fixture.layout().current_release_selection()).unwrap();
    let before = fixture.protected();
    let value = fixture.stage();
    assert!(!fixture.layout().current_release_selection().exists());
    let error = run(value, |_, _| panic!("must refuse before mutation"))
        .err()
        .unwrap();
    assert_eq!(error.reason(), "release is already selected");
    assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
    assert!(error.receipt_residues().is_empty());
    fixture.unchanged(before);
}

#[test]
fn preexisting_final_directory_or_receipt_is_never_adopted() {
    for receipt in [false, true] {
        for kind in 0..5 {
            let fixture = Fixture::new();
            let value = fixture.stage();
            let path = if receipt {
                fixture.receipt()
            } else {
                fixture.directory()
            };
            let mut _socket = None;
            let mut _socket_home = None;
            match kind {
                0 => fs::write(&path, b"existing").unwrap(),
                1 => {
                    fs::create_dir(&path).unwrap();
                    chmod(&path, 0o700);
                }
                2 => symlink("missing", &path).unwrap(),
                3 => {
                    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                _ => {
                    let temporary = tempfile::tempdir().unwrap();
                    let socket = temporary.path().join("s");
                    _socket = Some(UnixListener::bind(&socket).unwrap());
                    fs::rename(socket, &path).unwrap();
                    _socket_home = Some(temporary);
                }
            }
            let before = Fingerprint::from(fs::symlink_metadata(&path).unwrap());
            let error = run(value, |_, _| panic!("must refuse before mutation"))
                .err()
                .unwrap();
            assert_eq!(error.reason(), "publication destination already exists");
            assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
            assert!(error.receipt_residues().is_empty());
            assert_eq!(
                before,
                Fingerprint::from(fs::symlink_metadata(path).unwrap())
            );
        }
    }
}

#[test]
fn absent_current_stays_absent_after_publication() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.layout().current_release_selection()).unwrap();
    let published = run(fixture.stage(), |_, _| Ok(())).unwrap();
    assert_eq!(published.directory_name(), fixture.name());
    assert!(fs::symlink_metadata(fixture.layout().current_release_selection()).is_err());
}

#[test]
fn exclusive_create_and_rename_collisions_preserve_competing_entries() {
    for point in [
        Point::BeforeCreateCall,
        Point::BeforeReceiptRenameCall,
        Point::BeforeStageRenameCall,
    ] {
        let fixture = Fixture::new();
        let value = fixture.stage();
        let old = value.directory_name().to_owned();
        let target = match point {
            Point::BeforeCreateCall => fixture.layout().releases_directory().join(TEMPORARY),
            Point::BeforeReceiptRenameCall => fixture.receipt(),
            _ => fixture.directory(),
        };
        let mut before = None;
        let error = run(value, |at, _| {
            if at == point {
                if point == Point::BeforeStageRenameCall {
                    fs::create_dir(&target).unwrap();
                    chmod(&target, 0o700);
                } else {
                    fs::write(&target, b"competing-entry").unwrap();
                    chmod(&target, 0o600);
                }
                before = Some(Fingerprint::from(fs::symlink_metadata(&target).unwrap()));
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
        assert_eq!(error.directory_residues(), [old]);
        assert_eq!(
            before.unwrap(),
            Fingerprint::from(fs::symlink_metadata(&target).unwrap())
        );
        let expected = match point {
            Point::BeforeCreateCall => ("publication receipt creation failed", Vec::new()),
            Point::BeforeReceiptRenameCall => (
                "publication receipt rename failed",
                vec![TEMPORARY.to_owned()],
            ),
            _ => (
                "publication stage rename failed",
                vec![fixture.receipt_name()],
            ),
        };
        assert_eq!(error.reason(), expected.0);
        assert_eq!(error.receipt_residues(), expected.1);
        if point != Point::BeforeStageRenameCall {
            assert_eq!(fs::read(target).unwrap(), b"competing-entry");
        }
    }
}

#[test]
fn failures_report_precise_phase_and_residue_without_cleanup() {
    use PublicationPhase::{AfterCommit, BeforeCommit};
    for (point, phase, receipt) in [
        (Point::BeforeCreate, BeforeCommit, 0),
        (Point::BeforeCreateCall, BeforeCommit, 0),
        (Point::ReceiptCreated, BeforeCommit, 1),
        (Point::BeforeWrite, BeforeCommit, 1),
        (Point::ReceiptWritten, BeforeCommit, 1),
        (Point::BeforeChmod, BeforeCommit, 1),
        (Point::ReceiptReadonly, BeforeCommit, 1),
        (Point::BeforeReceiptSync, BeforeCommit, 1),
        (Point::ReceiptClosed, BeforeCommit, 1),
        (Point::BeforeReceiptRename, BeforeCommit, 1),
        (Point::BeforeReceiptRenameCall, BeforeCommit, 1),
        (Point::ReceiptRenamed, BeforeCommit, 2),
        (Point::BeforeReceiptParentSync, BeforeCommit, 2),
        (Point::BeforeStageRename, BeforeCommit, 2),
        (Point::BeforeStageRenameCall, BeforeCommit, 2),
        (Point::StageRenamed, AfterCommit, 2),
        (Point::BeforeFinalVerify, AfterCommit, 2),
        (Point::AfterFinalVerify, AfterCommit, 2),
        (Point::BeforeDirectorySync, AfterCommit, 2),
        (Point::BeforeFinalParentSync, AfterCommit, 2),
        (Point::Complete, AfterCommit, 2),
    ] {
        let fixture = Fixture::new();
        let before = fixture.protected();
        let value = fixture.stage();
        let old = value.directory_name().to_owned();
        let error = run(value, |at, _| {
            if at == point {
                Err("fixture publication fault")
            } else {
                Ok(())
            }
        })
        .err()
        .unwrap();
        assert_eq!(error.reason(), "fixture publication fault");
        assert_eq!(error.phase(), phase);
        let directory = if phase == AfterCommit {
            fixture.name()
        } else {
            old
        };
        assert_eq!(error.directory_residues(), std::slice::from_ref(&directory));
        assert!(
            fixture
                .layout()
                .releases_directory()
                .join(directory)
                .is_dir()
        );
        let expected = match receipt {
            0 => Vec::new(),
            1 => vec![TEMPORARY.to_owned()],
            _ => vec![fixture.receipt_name()],
        };
        assert_eq!(error.receipt_residues(), expected);
        for name in expected {
            assert!(fixture.layout().releases_directory().join(name).is_file());
        }
        fixture.unchanged(before);
    }
}

#[test]
fn unsupported_exclusive_rename_has_no_retry_fallback_or_adoption() {
    for which in [Rename::Receipt, Rename::Stage] {
        let fixture = Fixture::new();
        let value = fixture.stage();
        let old = value.directory_name().to_owned();
        let mut hooks = Hooks {
            check: Some(&mut |_, _| Ok(())),
            nonce: Some(NONCE),
            rename_error: Some((which, libc::ENOTSUP, false)),
            ..Hooks::production()
        };
        let error = publish(value, &mut hooks).err().unwrap();
        assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
        assert_eq!(error.directory_residues(), [old]);
        assert!(!fixture.directory().exists());
        assert_eq!(
            hooks.renames,
            if which == Rename::Receipt {
                vec![Rename::Receipt]
            } else {
                vec![Rename::Receipt, Rename::Stage]
            }
        );
        assert_eq!(
            error.receipt_residues(),
            [if which == Rename::Receipt {
                TEMPORARY.to_owned()
            } else {
                fixture.receipt_name()
            }]
        );
    }
}

#[test]
fn uncertain_rename_errors_keep_candidate_locators_on_both_sides() {
    for which in [Rename::Receipt, Rename::Stage] {
        for applied in [false, true] {
            let fixture = Fixture::new();
            let value = fixture.stage();
            let old = value.directory_name().to_owned();
            let error = publish(
                value,
                &mut Hooks {
                    check: Some(&mut |_, _| Ok(())),
                    nonce: Some(NONCE),
                    rename_error: Some((which, libc::EIO, applied)),
                    ..Hooks::production()
                },
            )
            .err()
            .unwrap();
            if which == Rename::Stage {
                assert_eq!(error.phase(), PublicationPhase::UncertainCommit);
                assert_eq!(error.directory_residues(), [old.clone(), fixture.name()]);
                assert_eq!(fixture.directory().exists(), applied);
                assert_eq!(
                    fixture.layout().releases_directory().join(old).exists(),
                    !applied
                );
                assert_eq!(error.receipt_residues(), [fixture.receipt_name()]);
            } else {
                assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
                assert_eq!(error.directory_residues(), [old]);
                assert_eq!(
                    error.receipt_residues(),
                    [TEMPORARY.to_owned(), fixture.receipt_name()]
                );
                assert_eq!(fixture.receipt().exists(), applied);
                assert!(!fixture.directory().exists());
            }
        }
    }
}

#[test]
fn signer_failures_before_and_after_commit_never_create_an_acceptance_result() {
    for failure in 0..6 {
        let fixture = Fixture::new();
        let before = fixture.protected();
        let value = fixture.stage();
        let mut calls = 0;
        let mut check = |_, _: &Path| {
            let index = calls;
            calls += 1;
            if index == failure {
                Err("fixture signer rejection")
            } else {
                Ok(())
            }
        };
        let error = publish(
            value,
            &mut Hooks {
                check: Some(&mut check),
                nonce: Some(NONCE),
                ..Hooks::production()
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.reason(), "fixture signer rejection");
        assert_eq!(calls, failure + 1);
        assert_eq!(
            error.phase(),
            if failure < 3 {
                PublicationPhase::BeforeCommit
            } else {
                PublicationPhase::AfterCommit
            }
        );
        assert_eq!(fixture.directory().exists(), failure >= 3);
        assert_eq!(fixture.receipt().exists(), failure >= 3);
        fixture.unchanged(before);
    }
}

#[test]
fn same_target_current_replacement_is_detected_on_each_side_of_mutations() {
    for point in [
        Point::BeforeCreateCall,
        Point::BeforeReceiptRenameCall,
        Point::BeforeStageRenameCall,
    ] {
        let fixture = Fixture::new();
        let value = fixture.stage();
        let mut changed = None;
        let error = run(value, |at, _| {
            if at == point {
                let current = fixture.layout().current_release_selection();
                let target = fs::read_link(&current).unwrap();
                fs::rename(&current, current.with_extension("old")).unwrap();
                symlink(target, &current).unwrap();
                changed = Some(Fingerprint::from(fs::symlink_metadata(current).unwrap()));
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(
            error.reason(),
            "unsafe or changed current-release selection"
        );
        assert_eq!(
            error.phase(),
            if point == Point::BeforeStageRenameCall {
                PublicationPhase::AfterCommit
            } else {
                PublicationPhase::BeforeCommit
            }
        );
        assert_eq!(
            changed.unwrap(),
            Fingerprint::from(
                fs::symlink_metadata(fixture.layout().current_release_selection()).unwrap()
            )
        );
    }
}

#[test]
fn receipt_corruption_replacement_links_modes_and_acls_are_refused() {
    for point in [
        Point::ReceiptCreated,
        Point::ReceiptClosed,
        Point::ReceiptRenamed,
        Point::Complete,
    ] {
        for mutation in 0..6 {
            let fixture = Fixture::new();
            let value = fixture.stage();
            let mut reached = false;
            let error = run(value, |at, parent| {
                if at == point {
                    reached = true;
                    let path = if matches!(point, Point::ReceiptRenamed | Point::Complete) {
                        fixture.receipt()
                    } else {
                        parent.join(TEMPORARY)
                    };
                    let expected_mode = if point == Point::ReceiptCreated {
                        0o600
                    } else {
                        0o400
                    };
                    match mutation {
                        0 => {
                            chmod(&path, 0o600);
                            fs::write(&path, b"changed receipt").unwrap();
                            chmod(&path, expected_mode);
                        }
                        1 => chmod(&path, 0o644),
                        2 => acl(&path),
                        3 => fs::hard_link(&path, parent.join("receipt-link")).unwrap(),
                        4 => {
                            fs::rename(&path, parent.join("held-receipt")).unwrap();
                            symlink("missing", &path).unwrap();
                        }
                        _ => {
                            let bytes = fs::read(&path).unwrap();
                            fs::rename(&path, parent.join("held-receipt")).unwrap();
                            fs::write(&path, bytes).unwrap();
                            chmod(&path, expected_mode);
                        }
                    }
                }
                Ok(())
            })
            .err()
            .unwrap();
            assert!(reached);
            assert_eq!(
                error.phase(),
                if point == Point::Complete {
                    PublicationPhase::AfterCommit
                } else {
                    PublicationPhase::BeforeCommit
                }
            );
            assert_eq!(fixture.directory().exists(), point == Point::Complete);
        }
    }
}

#[test]
fn stage_replacement_cannot_be_rebound_or_adopted_after_commit() {
    let fixture = Fixture::new();
    let value = fixture.stage();
    let old = value.directory_name().to_owned();
    let error = run(value, |point, parent| {
        if point == Point::BeforeStageRenameCall {
            let path = parent.join(&old);
            fs::rename(&path, parent.join("held-stage")).unwrap();
            fs::create_dir(&path).unwrap();
            chmod(&path, 0o700);
            for name in NAMES {
                fs::copy(fixture.source.file(name), path.join(name.filename())).unwrap();
            }
        }
        Ok(())
    })
    .err()
    .unwrap();
    assert_eq!(error.phase(), PublicationPhase::AfterCommit);
    assert!(fixture.directory().is_dir());
    assert!(
        fixture
            .layout()
            .releases_directory()
            .join("held-stage")
            .is_dir()
    );
}

#[test]
fn artifact_mutation_during_final_signer_or_after_sync_is_rejected() {
    for after_sync in [false, true] {
        let fixture = Fixture::new();
        let value = fixture.stage();
        let mut calls = 0;
        let mutate = || {
            let file = fixture.directory().join(ArtifactName::Daemon.filename());
            chmod(&file, 0o700);
            fs::write(&file, b"changed executable bytes").unwrap();
            chmod(&file, 0o500);
        };
        let mut check = |_, _: &Path| {
            calls += 1;
            if !after_sync && calls == 6 {
                mutate();
            }
            Ok(())
        };
        let mut fault = |point, _: &Path| {
            if after_sync && point == Point::Complete {
                let file = fixture.directory().join(ArtifactName::Daemon.filename());
                chmod(&file, 0o700);
                fs::write(&file, b"changed executable bytes").unwrap();
                chmod(&file, 0o500);
            }
            Ok(())
        };
        let error = publish(
            value,
            &mut Hooks {
                check: Some(&mut check),
                fault: Some(&mut fault),
                nonce: Some(NONCE),
                ..Hooks::production()
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.phase(), PublicationPhase::AfterCommit);
    }
}

#[test]
fn parent_stage_artifact_and_lock_access_changes_stop_before_commit() {
    for change in 0..5 {
        let fixture = Fixture::new();
        let value = fixture.stage();
        let stage_name = value.directory_name().to_owned();
        let mut reached = false;
        let error = run(value, |point, parent| {
            if point == Point::ReceiptCreated {
                reached = true;
                match change {
                    0 => chmod(parent, 0o750),
                    1 => acl(parent),
                    2 => chmod(&parent.join(&stage_name), 0o750),
                    3 => chmod(
                        &parent
                            .join(&stage_name)
                            .join(ArtifactName::Daemon.filename()),
                        0o700,
                    ),
                    _ => chmod(&fixture.layout().process_lock(), 0o644),
                }
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert!(reached);
        assert_eq!(error.phase(), PublicationPhase::BeforeCommit);
        assert_eq!(error.receipt_residues(), [TEMPORARY.to_owned()]);
        assert!(!fixture.directory().exists());
        assert!(!fixture.receipt().exists());
    }
}

const CHILD_HOME: &str = "HANDKITE_TEST_PUBLICATION_HOME";
const CHILD_SOURCE: &str = "HANDKITE_TEST_PUBLICATION_SOURCE";
const CHILD_SIDE: &str = "HANDKITE_TEST_PUBLICATION_SIDE";

#[test]
#[ignore = "invoked only by the publication interruption fixture"]
fn publication_child() {
    let layout = layout_at(std::env::var_os(CHILD_HOME).unwrap());
    let source = std::path::PathBuf::from(std::env::var_os(CHILD_SOURCE).unwrap());
    let bytes = manifest(&source);
    let owner = InstallerLock::acquire(&layout).unwrap();
    let value = staged(&owner, &source, &bytes);
    let stop = if std::env::var(CHILD_SIDE).unwrap() == "before" {
        Point::BeforeStageRename
    } else {
        Point::StageRenamed
    };
    run(value, |point, _| {
        if point == stop {
            println!("ready");
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        }
        Ok(())
    })
    .unwrap();
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn interruption_before_and_after_commit_leaves_expected_residue_and_same_lock_inode() {
    for side in ["before", "after"] {
        let fixture = Fixture::new();
        let before = fixture.protected();
        let layout = fixture.layout().clone();
        let digest = fixture.name();
        let receipt = fixture.receipt_name();
        let lock_before = Fingerprint::from(fs::metadata(layout.process_lock()).unwrap());
        let Fixture {
            _home,
            source,
            owner,
            bytes,
        } = fixture;
        drop(owner);
        let mut child = OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "release_stage::publication::macos::tests::publication_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD_HOME, layout.root().ancestors().nth(3).unwrap())
                .env(CHILD_SOURCE, &source.stage)
                .env(CHILD_SIDE, side)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (sender, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line == "ready" {
                    let _ = sender.send(());
                }
            }
        });
        received.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(InstallerLock::acquire(&layout).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let owner = InstallerLock::acquire(&layout).unwrap();
        owner.revalidate().unwrap();
        assert_eq!(
            lock_before,
            Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
        );
        assert_eq!(
            fs::read(layout.releases_directory().join(receipt)).unwrap(),
            bytes
        );
        assert_eq!(
            layout.releases_directory().join(digest).is_dir(),
            side == "after"
        );
        let stages = fs::read_dir(layout.releases_directory())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".stage-")
            })
            .count();
        assert_eq!(stages, usize::from(side == "before"));
        for (path, fingerprint) in before {
            assert_eq!(
                fingerprint,
                Fingerprint::from(fs::symlink_metadata(path).unwrap())
            );
        }
        drop(owner);
        drop(_home);
    }
}
