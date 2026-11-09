use super::*;
use crate::release_files::tests::{Fixture as SourceFixture, chmod};
use crate::release_manifest::{ReleaseManifest, RustIdentity, SdkIdentity};
use crate::service_layout::tests::{fixture_in, layout_at};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::symlink;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CERTIFICATE: &str = "1111111111111111111111111111111111111111";
const NONCE: [u8; 16] = [0x13; 16];
const NAME: &str = ".stage-13131313131313131313131313131313";

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

struct Fixture {
    _home: tempfile::TempDir,
    source: SourceFixture,
    owner: InstallerLock,
    manifest: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let (home, layout) = fixture_in(&std::env::current_dir().unwrap());
        let source = SourceFixture::new();
        for (index, name) in NAMES.into_iter().enumerate() {
            chmod(&source.file(name), 0o700);
            fs::write(source.file(name), vec![index as u8 + 1; 64 * 1024 + 17]).unwrap();
            chmod(&source.file(name), 0o500);
        }
        let manifest = manifest(&source.stage);
        Self {
            _home: home,
            source,
            owner: InstallerLock::acquire(&layout).unwrap(),
            manifest,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        self.owner.layout().releases_directory().join(NAME)
    }
}

fn run(
    fixture: &Fixture,
    mut fault: impl FnMut(Point, &Path) -> Result<(), &'static str>,
) -> Result<StagedRelease<'_>, StageFailure> {
    let mut check = |_, _: &Path| Ok(());
    stage(
        &fixture.owner,
        &fixture.source.stage,
        &fixture.manifest,
        &hash(&fixture.manifest),
        CERTIFICATE,
        &mut Hooks {
            check: Some(&mut check),
            fault: Some(&mut fault),
            nonce: Some(NONCE),
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
fn copies_exact_bytes_modes_and_held_inodes_without_changing_other_state() {
    let fixture = Fixture::new();
    let layout = fixture.owner.layout();
    let paths = [
        layout.config_file(),
        layout.state_directory().join("UNLOCK-DISABLED"),
        layout.state_directory().join("state.json"),
    ];
    for path in &paths {
        fs::write(path, b"existing-state\n").unwrap();
        chmod(path, 0o600);
    }
    symlink("a".repeat(64), layout.current_release_selection()).unwrap();
    let snapshots: Vec<_> = paths
        .iter()
        .chain([&layout.current_release_selection()])
        .map(|path| Fingerprint::from(fs::symlink_metadata(path).unwrap()))
        .collect();
    let source_before: Vec<_> = NAMES
        .map(|name| Fingerprint::from(fs::metadata(fixture.source.file(name)).unwrap()))
        .into();
    let mut checks = Vec::new();
    let mut check = |name, path: &Path| {
        checks.push((name, path.to_path_buf()));
        Ok(())
    };
    let staged = stage(
        &fixture.owner,
        &fixture.source.stage,
        &fixture.manifest,
        &hash(&fixture.manifest),
        CERTIFICATE,
        &mut Hooks {
            check: Some(&mut check),
            fault: None,
            nonce: Some(NONCE),
        },
    )
    .unwrap();
    assert_eq!(staged.directory_name(), NAME);
    assert_eq!(checks.len(), 6);
    for (index, (name, path)) in checks.iter().enumerate() {
        assert_eq!(*name, NAMES[index % 3]);
        assert_eq!(
            path,
            &if index < 3 {
                fixture.source.file(*name)
            } else {
                fixture.path().join(name.filename())
            }
        );
    }
    for (index, name) in NAMES.into_iter().enumerate() {
        let destination = fixture.path().join(name.filename());
        assert_eq!(
            fs::read(&destination).unwrap(),
            fs::read(fixture.source.file(name)).unwrap()
        );
        let meta = fs::metadata(&destination).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o500);
        assert_eq!(meta.nlink(), 1);
        assert_eq!(
            (meta.dev(), meta.ino()),
            identity(&staged._destination.artifacts[index].file).unwrap()
        );
        assert_eq!(
            unsafe {
                libc::fcntl(
                    staged._destination.artifacts[index].file.as_raw_fd(),
                    libc::F_GETFL,
                )
            } & libc::O_ACCMODE,
            libc::O_RDONLY
        );
        assert_eq!(
            source_before[index],
            Fingerprint::from(fs::metadata(fixture.source.file(name)).unwrap())
        );
    }
    assert_eq!(fs::metadata(fixture.path()).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 3);
    for (path, before) in paths
        .iter()
        .chain([&layout.current_release_selection()])
        .zip(snapshots)
    {
        assert_eq!(
            before,
            Fingerprint::from(fs::symlink_metadata(path).unwrap())
        );
    }
    for path in paths {
        assert_eq!(fs::read(path).unwrap(), b"existing-state\n");
    }
    fixture.owner.revalidate().unwrap();
    drop(staged);
    assert!(fixture.path().is_dir());
}

#[test]
fn invalid_trust_and_unsigned_public_source_never_create_residue() {
    let fixture = Fixture::new();
    for (bytes, pin, cert, expected) in [
        (
            b"{}".as_slice(),
            [0; 32],
            CERTIFICATE,
            "invalid release manifest JSON",
        ),
        (
            fixture.manifest.as_slice(),
            [0; 32],
            CERTIFICATE,
            "release manifest pin mismatch",
        ),
        (
            fixture.manifest.as_slice(),
            hash(&fixture.manifest),
            "bad",
            "invalid signer certificate SHA1",
        ),
    ] {
        let error = stage(
            &fixture.owner,
            &fixture.source.stage,
            bytes,
            &pin,
            cert,
            &mut Hooks {
                check: Some(&mut |_, _| panic!("signer must not run")),
                fault: Some(&mut |_, _| panic!("creation must not run")),
                nonce: Some(NONCE),
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.reason(), expected);
        assert_eq!(error.residue_directory_name(), None);
    }
    let error = super::super::stage(
        &fixture.owner,
        &fixture.source.stage,
        &fixture.manifest,
        &hash(&fixture.manifest),
        CERTIFICATE,
    )
    .err()
    .unwrap();
    assert_eq!(
        error.reason(),
        "artifact signature does not satisfy signer policy"
    );
    assert_eq!(error.residue_directory_name(), None);
    assert_eq!(
        fs::read_dir(fixture.owner.layout().releases_directory())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn rejects_source_or_source_parent_overlap_before_mkdir() {
    for nested in [false, true] {
        let fixture = Fixture::new();
        let releases = fixture.owner.layout().releases_directory();
        let source = if nested {
            releases.join("source")
        } else {
            releases.clone()
        };
        if nested {
            fs::create_dir(&source).unwrap();
            chmod(&source, 0o700);
        }
        for name in NAMES {
            fs::copy(fixture.source.file(name), source.join(name.filename())).unwrap();
        }
        let mut check = |_, _: &Path| Ok(());
        let error = stage(
            &fixture.owner,
            &source,
            &fixture.manifest,
            &hash(&fixture.manifest),
            CERTIFICATE,
            &mut Hooks {
                check: Some(&mut check),
                fault: Some(&mut |_, _| panic!("must reject before creation")),
                nonce: Some(NONCE),
            },
        )
        .err()
        .unwrap();
        assert_eq!(
            error.reason(),
            "release source overlaps destination directory"
        );
        assert_eq!(error.residue_directory_name(), None);
        assert!(!fixture.path().exists());
    }
}

#[test]
fn collision_never_adopts_retries_or_reports_an_existing_entry_as_residue() {
    for kind in 0..3 {
        let fixture = Fixture::new();
        match kind {
            0 => {
                fs::create_dir(fixture.path()).unwrap();
                chmod(&fixture.path(), 0o700);
            }
            1 => fs::write(fixture.path(), b"existing-entry").unwrap(),
            _ => symlink(&fixture.source.stage, fixture.path()).unwrap(),
        }
        let before = Fingerprint::from(fs::symlink_metadata(fixture.path()).unwrap());
        let error = run(&fixture, |_, _| Ok(())).err().unwrap();
        assert_eq!(error.reason(), "release stage creation failed");
        assert_eq!(error.residue_directory_name(), None);
        assert_eq!(
            before,
            Fingerprint::from(fs::symlink_metadata(fixture.path()).unwrap())
        );
        assert_eq!(
            fs::read_dir(fixture.owner.layout().releases_directory())
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn exclusive_file_creation_never_overwrites_an_intervening_entry() {
    let fixture = Fixture::new();
    let mut before = None;
    let error = run(&fixture, |point, path| {
        if point == Point::BeforeFileCreate(ArtifactName::Daemon) {
            let file = path.join(ArtifactName::Daemon.filename());
            fs::write(&file, b"intervening-entry").unwrap();
            chmod(&file, 0o600);
            before = Some(Fingerprint::from(fs::metadata(file).unwrap()));
        }
        Ok(())
    })
    .err()
    .unwrap();
    assert_eq!(error.reason(), COPY_FAILED);
    assert_eq!(error.residue_directory_name(), Some(NAME));
    let file = fixture.path().join(ArtifactName::Daemon.filename());
    assert_eq!(
        before.unwrap(),
        Fingerprint::from(fs::metadata(&file).unwrap())
    );
    assert_eq!(fs::read(file).unwrap(), b"intervening-entry");
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn signer_rejection_stops_in_order_with_residue_only_for_destination_checks() {
    for failure in 0..6 {
        let fixture = Fixture::new();
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
        let error = stage(
            &fixture.owner,
            &fixture.source.stage,
            &fixture.manifest,
            &hash(&fixture.manifest),
            CERTIFICATE,
            &mut Hooks {
                check: Some(&mut check),
                fault: None,
                nonce: Some(NONCE),
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.reason(), "fixture signer rejection");
        assert_eq!(calls, failure + 1);
        assert_eq!(
            error.residue_directory_name(),
            (failure >= 3).then_some(NAME)
        );
        assert_eq!(fixture.path().exists(), failure >= 3);
    }
}

#[test]
fn unsafe_source_destination_parent_or_owner_fail_before_creation() {
    for change in 0..6 {
        let fixture = Fixture::new();
        match change {
            0 => chmod(&fixture.source.file(ArtifactName::Daemon), 0o700),
            1 => acl(&fixture.source.file(ArtifactName::Daemon)),
            2 => chmod(&fixture.owner.layout().releases_directory(), 0o750),
            3 => acl(&fixture.owner.layout().releases_directory()),
            4 => {
                let path = fixture.owner.layout().releases_directory();
                fs::rename(&path, path.with_extension("old")).unwrap();
                symlink(path.with_extension("old"), &path).unwrap();
            }
            _ => chmod(&fixture.owner.layout().process_lock(), 0o644),
        }
        let error = run(&fixture, |_, _| panic!("must fail before creation"))
            .err()
            .unwrap();
        assert_eq!(error.residue_directory_name(), None);
        assert!(!fixture.path().exists());
    }
}

#[test]
fn injected_copy_sync_and_late_failures_leave_only_explicit_residue() {
    for fail_at in [
        Point::BeforeCreate,
        Point::Created,
        Point::FileCreated(ArtifactName::Daemon),
        Point::BeforeWrite(ArtifactName::Cli),
        Point::AfterWrite(ArtifactName::Mcp),
        Point::BeforeFileSync(ArtifactName::Daemon),
        Point::FileClosed(ArtifactName::Cli),
        Point::BeforeStageSync,
        Point::BeforeReleasesSync,
        Point::BeforeDestinationOpen,
        Point::DestinationVerified,
    ] {
        let fixture = Fixture::new();
        let layout = fixture.owner.layout();
        let protected = [
            layout.config_file(),
            layout.state_directory().join("UNLOCK-DISABLED"),
            layout.state_directory().join("state.json"),
        ];
        for path in &protected {
            fs::write(path, b"preserved-state").unwrap();
            chmod(path, 0o600);
        }
        symlink("a".repeat(64), layout.current_release_selection()).unwrap();
        let before: Vec<_> = protected
            .iter()
            .chain([&layout.current_release_selection()])
            .map(|path| Fingerprint::from(fs::symlink_metadata(path).unwrap()))
            .collect();
        let error = run(&fixture, |point, _| {
            if point == fail_at {
                Err("fixture I/O failure")
            } else {
                Ok(())
            }
        })
        .err()
        .unwrap();
        assert_eq!(error.reason(), "fixture I/O failure");
        assert_eq!(
            error.residue_directory_name(),
            (fail_at != Point::BeforeCreate).then_some(NAME)
        );
        if fail_at == Point::BeforeCreate {
            assert!(!fixture.path().exists());
        } else {
            assert!(fixture.path().is_dir());
            assert!(fs::read_dir(fixture.path()).unwrap().count() <= 3);
        }
        fixture.owner.revalidate().unwrap();
        for (path, before) in protected
            .iter()
            .chain([&layout.current_release_selection()])
            .zip(before)
        {
            assert_eq!(
                before,
                Fingerprint::from(fs::symlink_metadata(path).unwrap())
            );
        }
        for path in &protected {
            assert_eq!(fs::read(path).unwrap(), b"preserved-state");
        }
    }
}

#[test]
fn source_growth_shrink_and_corruption_are_bounded_copy_failures() {
    for bytes in [
        vec![8; 64 * 1024 + 18],
        b"short".to_vec(),
        vec![9; 64 * 1024 + 17],
    ] {
        let fixture = Fixture::new();
        let error = run(&fixture, |point, _| {
            if point == Point::BeforeWrite(ArtifactName::Daemon) {
                let source = fixture.source.file(ArtifactName::Daemon);
                chmod(&source, 0o700);
                fs::write(&source, &bytes).unwrap();
                chmod(&source, 0o500);
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.reason(), COPY_FAILED);
        assert_eq!(error.residue_directory_name(), Some(NAME));
        assert!(
            fs::metadata(fixture.path().join(ArtifactName::Daemon.filename()))
                .unwrap()
                .len()
                <= 64 * 1024 + 17
        );
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    }
}

#[test]
fn destination_corruption_and_same_byte_replacement_are_refused() {
    for change in 0..6 {
        let fixture = Fixture::new();
        let error = run(&fixture, |point, path| {
            if point == Point::BeforeDestinationOpen {
                let file = path.join(ArtifactName::Daemon.filename());
                match change {
                    0 => {
                        chmod(&file, 0o700);
                        fs::write(&file, vec![9; 64 * 1024 + 17]).unwrap();
                        chmod(&file, 0o500);
                    }
                    1 => {
                        fs::rename(&file, path.join("old")).unwrap();
                        fs::copy(fixture.source.file(ArtifactName::Daemon), &file).unwrap();
                        fs::remove_file(path.join("old")).unwrap();
                    }
                    2 => chmod(&file, 0o700),
                    3 => acl(&file),
                    4 => fs::hard_link(&file, path.join("extra-link")).unwrap(),
                    _ => {
                        fs::remove_file(&file).unwrap();
                        symlink(fixture.source.file(ArtifactName::Daemon), &file).unwrap();
                    }
                }
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.residue_directory_name(), Some(NAME));
    }
}

#[test]
fn directory_and_lock_races_never_succeed_or_clean_up_residue() {
    for change in 0..6 {
        let fixture = Fixture::new();
        let error = run(&fixture, |point, path| {
            if point == Point::Created {
                match change {
                    0 => {
                        fs::rename(path, path.with_extension("old")).unwrap();
                        fs::create_dir(path).unwrap();
                        chmod(path, 0o700);
                    }
                    1 => chmod(path, 0o750),
                    2 => acl(path),
                    3 => {
                        let releases = fixture.owner.layout().releases_directory();
                        fs::rename(&releases, releases.with_extension("old")).unwrap();
                        fs::create_dir(&releases).unwrap();
                        chmod(&releases, 0o700);
                    }
                    4 => fs::write(path.join("unexpected"), b"existing").unwrap(),
                    _ => chmod(&fixture.owner.layout().process_lock(), 0o644),
                }
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.residue_directory_name(), Some(NAME));
        assert!(
            fixture.path().exists()
                || fixture
                    .owner
                    .layout()
                    .releases_directory()
                    .with_extension("old")
                    .join(NAME)
                    .exists()
        );
    }
}

#[test]
fn final_validation_rechecks_both_source_and_destination_after_signers() {
    for change in 0..3 {
        let fixture = Fixture::new();
        let error = run(&fixture, |point, path| {
            if point == Point::DestinationVerified {
                let file = match change {
                    0 => fixture.source.file(ArtifactName::Daemon),
                    1 => path.join(ArtifactName::Daemon.filename()),
                    _ => fixture.owner.layout().process_lock(),
                };
                chmod(&file, 0o700);
                fs::write(&file, b"changed").unwrap();
                chmod(&file, if change == 2 { 0o600 } else { 0o500 });
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.residue_directory_name(), Some(NAME));
    }
}

#[test]
fn production_names_are_bounded_random_relative_locators() {
    let hooks = Hooks::production();
    let first = new_name(&hooks).unwrap();
    let second = new_name(&hooks).unwrap();
    assert_ne!(first, second);
    for name in [first, second] {
        assert_eq!(name.len(), 39);
        assert!(name.starts_with(".stage-"));
        assert!(
            name[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }
}

#[test]
fn staged_custody_retains_exact_bounded_manifest_and_the_used_trust_inputs() {
    let fixture = Fixture::new();
    let mut supplied = fixture.manifest.clone();
    let mut staged = stage(
        &fixture.owner,
        &fixture.source.stage,
        &supplied,
        &hash(&supplied),
        CERTIFICATE,
        &mut Hooks {
            check: Some(&mut |_, _| Ok(())),
            fault: None,
            nonce: Some(NONCE),
        },
    )
    .unwrap();
    supplied.fill(b' ');
    assert_eq!(
        staged._canonical_manifest.as_ref(),
        fixture.manifest.as_slice()
    );
    assert!(staged._canonical_manifest.len() <= crate::release_manifest::MAX_MANIFEST_BYTES);
    assert_eq!(
        staged._trust.manifest().canonical_json().unwrap(),
        fixture.manifest
    );
    assert!(std::ptr::eq(staged._owner, &fixture.owner));
    rehash_held(&mut staged._destination, &staged._trust).unwrap();
    assert_eq!(
        verify_held(&mut staged._destination, &staged._trust, None),
        Err("artifact signature does not satisfy signer policy")
    );
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 3);
}

const CHILD_HOME: &str = "HANDKITE_TEST_STAGE_HOME";
const CHILD_SOURCE: &str = "HANDKITE_TEST_STAGE_SOURCE";

#[test]
#[ignore = "invoked only by the interruption fixture"]
fn interrupted_child() {
    let layout = layout_at(std::env::var_os(CHILD_HOME).unwrap());
    let source = std::path::PathBuf::from(std::env::var_os(CHILD_SOURCE).unwrap());
    let bytes = manifest(&source);
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut fault = |point, _: &Path| {
        if point == Point::FileClosed(ArtifactName::Daemon) {
            println!("created");
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
        }
        Ok(())
    };
    stage(
        &owner,
        &source,
        &bytes,
        &hash(&bytes),
        CERTIFICATE,
        &mut Hooks {
            check: Some(&mut |_, _| Ok(())),
            fault: Some(&mut fault),
            nonce: Some(NONCE),
        },
    )
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
fn interruption_leaves_partial_stage_and_releases_only_the_owned_lock() {
    let (home, layout) = fixture_in(&std::env::current_dir().unwrap());
    let source = SourceFixture::new();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let lock_before = Fingerprint::from(fs::metadata(layout.process_lock()).unwrap());
    drop(owner);
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "release_stage::macos::tests::interrupted_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_HOME, layout.root().ancestors().nth(3).unwrap())
            .env(CHILD_SOURCE, &source.stage)
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
            if line == "created" {
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
    let residue = layout.releases_directory().join(NAME);
    assert_eq!(fs::read_dir(&residue).unwrap().count(), 1);
    assert_eq!(
        fs::read(residue.join(ArtifactName::Daemon.filename())).unwrap(),
        fs::read(source.file(ArtifactName::Daemon)).unwrap()
    );
    drop(owner);
    drop(home);
}
