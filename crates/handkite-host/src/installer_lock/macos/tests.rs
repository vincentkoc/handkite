use super::*;
use crate::installer_lock::InstallerLock;
use crate::service_layout::tests::{fixture_in, layout_at};
use crate::service_lock::ServiceLock;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn fixture() -> (tempfile::TempDir, ServiceLayout) {
    fixture_in(&std::env::current_dir().unwrap())
}

fn lock_file(layout: &ServiceLayout) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(layout.process_lock())
        .unwrap()
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn acl(path: &Path, rule: &str) {
    assert!(
        Command::new("/bin/chmod")
            .args(["+a", rule])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

fn clear_acl(path: &Path) {
    assert!(
        Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn user_job_admission_requires_absence_and_preserves_private_state() {
    use crate::launchd_observation::Observation;

    let (_scratch, layout) = fixture();
    let paths = [
        layout.config_file(),
        layout.state_directory().join("UNLOCK-DISABLED"),
    ];
    for path in &paths {
        fs::write(path, b"preserve-private-state").unwrap();
        chmod(path, 0o600);
    }
    let owner = InstallerLock::acquire(&layout).unwrap();
    let before: Vec<_> = paths
        .iter()
        .map(|path| Fingerprint::from(fs::symlink_metadata(path).unwrap()))
        .collect();
    for observation in [
        Observation::Absent,
        Observation::Loaded,
        Observation::Unknown,
    ] {
        owner.set_user_job_for_test(observation);
        assert_eq!(
            owner.require_user_job_absent(),
            if observation == Observation::Absent {
                Ok(())
            } else {
                Err(crate::installer_lock::JOB_UNAVAILABLE)
            }
        );
    }
    for (path, fingerprint) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), b"preserve-private-state");
        assert_eq!(fingerprint, fs::symlink_metadata(path).unwrap().into());
    }
}

#[test]
fn user_job_admission_rejects_lock_replacement_before_and_during_observation() {
    use crate::launchd_observation::Observation;

    for during in [false, true] {
        let (_scratch, layout) = fixture();
        let owner = InstallerLock::acquire(&layout).unwrap();
        let replace = || {
            fs::remove_file(layout.process_lock()).unwrap();
            drop(lock_file(&layout));
        };
        if !during {
            replace();
        }
        let mut observed = false;
        assert_eq!(
            owner.check_user_job_absent(|| {
                observed = true;
                replace();
                Observation::Absent
            }),
            Err(crate::installer_lock::UNAVAILABLE)
        );
        assert_eq!(observed, during);
    }
}

#[test]
fn preserves_existing_lock_and_all_other_private_state() {
    for existing in [false, true] {
        let (_scratch, layout) = fixture();
        let marker = layout.state_directory().join("UNLOCK-DISABLED");
        let state = layout.state_directory().join("state.json");
        for path in [&marker, &state, &layout.config_file()] {
            fs::write(path, b"existing-private-state\n").unwrap();
            chmod(path, 0o600);
        }
        symlink("a".repeat(64), layout.current_release_selection()).unwrap();
        let paths = [
            marker,
            state,
            layout.config_file(),
            layout.current_release_selection(),
            layout.state_directory(),
            layout.releases_directory(),
        ];
        let before: Vec<_> = paths
            .iter()
            .map(|path| Fingerprint::from(fs::symlink_metadata(path).unwrap()))
            .collect();
        let old = existing.then(|| {
            lock_file(&layout)
                .write_all(b"preserve-lock-bytes")
                .unwrap();
            Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
        });
        let guard = InstallerLock::acquire(&layout).unwrap();
        guard.revalidate().unwrap();
        assert!(InstallerLock::acquire(&layout).is_err());
        let locked = Fingerprint::from(fs::metadata(layout.process_lock()).unwrap());
        if let Some(old) = old {
            assert_eq!(old, locked);
        }
        drop(guard);
        let guard = InstallerLock::acquire(&layout).unwrap();
        guard.revalidate().unwrap();
        assert_eq!(
            locked,
            Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
        );
        assert_eq!(
            fs::read(layout.process_lock()).unwrap(),
            if existing {
                b"preserve-lock-bytes".as_slice()
            } else {
                b""
            }
        );
        for (path, before) in paths.iter().zip(before) {
            assert_eq!(
                before,
                Fingerprint::from(fs::symlink_metadata(path).unwrap())
            );
        }
        for path in &paths[..3] {
            assert_eq!(fs::read(path).unwrap(), b"existing-private-state\n");
        }
        assert_eq!(
            fs::read_link(layout.current_release_selection()).unwrap(),
            Path::new(&"a".repeat(64))
        );
    }
}

#[test]
fn refuses_unsafe_lock_types_modes_links_owners_and_acls() {
    let (_scratch, layout) = fixture();
    let file = lock_file(&layout);
    assert!(file_policy(&file, unsafe { libc::geteuid() }.wrapping_add(1)).is_err());
    for mode in [0o400, 0o644, 0o660, 0o1600, 0o4600] {
        chmod(&layout.process_lock(), mode);
        assert!(InstallerLock::acquire(&layout).is_err());
    }
    chmod(&layout.process_lock(), 0o600);
    for rule in ["everyone allow read", "everyone deny write"] {
        acl(&layout.process_lock(), rule);
        assert!(InstallerLock::acquire(&layout).is_err());
        clear_acl(&layout.process_lock());
    }
    let extra = layout.runtime_directory().join("other");
    fs::hard_link(layout.process_lock(), &extra).unwrap();
    assert!(InstallerLock::acquire(&layout).is_err());
    fs::remove_file(&extra).unwrap();
    fs::remove_file(layout.process_lock()).unwrap();
    symlink("missing", layout.process_lock()).unwrap();
    assert!(InstallerLock::acquire(&layout).is_err());
    assert_eq!(
        fs::read_link(layout.process_lock()).unwrap(),
        Path::new("missing")
    );
    fs::remove_file(layout.process_lock()).unwrap();
    fs::create_dir(layout.process_lock()).unwrap();
    assert!(InstallerLock::acquire(&layout).is_err());
    fs::remove_dir(layout.process_lock()).unwrap();
    let name = CString::new(layout.process_lock().as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(InstallerLock::acquire(&layout).is_err());
}

#[test]
fn refuses_unsafe_account_parents_and_requires_private_runtime() {
    let (_scratch, layout) = fixture();
    let mut path = layout.runtime_directory();
    for depth in 0..=4 {
        chmod(&path, 0o722);
        assert!(InstallerLock::acquire(&layout).is_err());
        chmod(&path, 0o700);
        acl(&path, "everyone allow read");
        assert!(InstallerLock::acquire(&layout).is_err());
        clear_acl(&path);
        let file = File::open(&path).unwrap();
        assert!(
            directory_policy(
                &file,
                unsafe { libc::geteuid() }.wrapping_add(1),
                depth == 0,
                true
            )
            .is_err()
        );
        if depth == 0 {
            chmod(&path, 0o750);
            assert!(InstallerLock::acquire(&layout).is_err());
            chmod(&path, 0o700);
            acl(&path, "everyone deny write");
            assert!(InstallerLock::acquire(&layout).is_err());
            clear_acl(&path);
        }
        path.pop();
    }
    assert!(!layout.process_lock().exists());
    let moved = layout.root().join("runtime-moved");
    fs::rename(layout.runtime_directory(), &moved).unwrap();
    symlink(&moved, layout.runtime_directory()).unwrap();
    assert!(InstallerLock::acquire(&layout).is_err());
    assert!(!moved.join("handkite.lock").exists());
}

#[test]
fn refuses_missing_or_symlinked_account_parents_without_creating_a_lock() {
    for depth in 0..=4 {
        let (_scratch, layout) = fixture();
        let path = layout
            .runtime_directory()
            .ancestors()
            .nth(depth)
            .unwrap()
            .to_path_buf();
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        assert!(InstallerLock::acquire(&layout).is_err());
        symlink(&moved, &path).unwrap();
        assert!(InstallerLock::acquire(&layout).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(&moved, &path).unwrap();
        assert!(!layout.process_lock().exists());
    }
}

#[test]
fn exclusive_creation_never_retries_or_replaces_a_competitor() {
    for point in [Point::AfterAbsent, Point::BeforeCreate] {
        let (_scratch, layout) = fixture();
        let mut competitor = None;
        let mut hook = |seen| {
            if seen == point {
                lock_file(&layout).write_all(b"competing-owner").unwrap();
                competitor = Some(Fingerprint::from(
                    fs::metadata(layout.process_lock()).unwrap(),
                ));
            }
        };
        assert!(acquire_inner(&layout, &mut hook).is_err());
        assert_eq!(
            competitor.unwrap(),
            Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
        );
        assert_eq!(fs::read(layout.process_lock()).unwrap(), b"competing-owner");
        InstallerLock::acquire(&layout)
            .unwrap()
            .revalidate()
            .unwrap();
    }
}

#[test]
fn refuses_path_and_access_changes_during_acquisition_without_cleanup() {
    for existing in [false, true] {
        for point in [Point::AfterOpen, Point::AfterFlock, Point::AfterBaseline] {
            for mutation in 0..6 {
                let (_scratch, layout) = fixture();
                if existing {
                    lock_file(&layout);
                }
                let mut hook = |seen| {
                    if seen != point {
                        return;
                    }
                    match mutation {
                        0 => {
                            fs::rename(
                                layout.process_lock(),
                                layout.runtime_directory().join("old-lock"),
                            )
                            .unwrap();
                            lock_file(&layout);
                        }
                        1 => chmod(&layout.process_lock(), 0o644),
                        2 => acl(&layout.process_lock(), "everyone allow read"),
                        3 => {
                            fs::rename(
                                layout.runtime_directory(),
                                layout.root().join("old-runtime"),
                            )
                            .unwrap();
                            fs::create_dir(layout.runtime_directory()).unwrap();
                            chmod(&layout.runtime_directory(), 0o700);
                            lock_file(&layout);
                        }
                        4 => chmod(&layout.runtime_directory(), 0o750),
                        _ => acl(&layout.runtime_directory(), "everyone allow read"),
                    }
                };
                assert!(acquire_inner(&layout, &mut hook).is_err());
                // Failed acquisition never unlinks a created or substituted entry.
                assert!(layout.process_lock().exists());
            }
        }
    }
}

#[test]
fn full_baseline_detects_later_runtime_file_and_ancestor_changes() {
    for mutation in 0..5 {
        let (_scratch, layout) = fixture();
        let guard = InstallerLock::acquire(&layout).unwrap();
        match mutation {
            0 => fs::write(layout.runtime_directory().join("unrelated"), b"changed").unwrap(),
            1 => fs::write(layout.process_lock(), b"changed").unwrap(),
            2 => {
                fs::rename(
                    layout.process_lock(),
                    layout.runtime_directory().join("old-lock"),
                )
                .unwrap();
                lock_file(&layout);
            }
            3 => {
                fs::rename(layout.root(), layout.root().with_extension("old")).unwrap();
                fs::create_dir(layout.root()).unwrap();
                chmod(&layout.root(), 0o700);
            }
            _ => acl(&layout.process_lock(), "everyone deny write"),
        }
        assert_eq!(guard.revalidate(), Err(UNAVAILABLE));
    }
    let (_scratch, layout) = fixture();
    assert!(
        acquire_inner(&layout, &mut |point| {
            if point == Point::AfterBaseline {
                fs::write(layout.runtime_directory().join("unrelated"), b"changed").unwrap();
            }
        })
        .is_err()
    );
}

const CHILD_HOME: &str = "HANDKITE_TEST_INSTALLER_HOME";
const CHILD_KIND: &str = "HANDKITE_TEST_INSTALLER_KIND";

#[test]
#[ignore = "invoked only by the subprocess ownership fixtures"]
fn lock_child() {
    let layout = layout_at(std::env::var_os(CHILD_HOME).unwrap());
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let installer;
    let service;
    let acquired = if std::env::var(CHILD_KIND).unwrap() == "installer" {
        installer = InstallerLock::acquire(&layout);
        installer.is_ok()
    } else {
        service = ServiceLock::acquire(layout);
        service.is_ok()
    };
    println!("{}", if acquired { "owned" } else { "refused" });
    std::io::stdout().flush().unwrap();
    if acquired {
        std::io::stdin().read_line(&mut line).unwrap();
    }
}

struct OwnedChild {
    child: Child,
    status: mpsc::Receiver<String>,
}

impl OwnedChild {
    fn spawn(layout: &ServiceLayout, kind: &str) -> Self {
        let home = layout.root().ancestors().nth(3).unwrap().to_path_buf();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer_lock::macos::tests::lock_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_HOME, home)
            .env(CHILD_KIND, kind)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, status) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line == "owned" || line == "refused" {
                    let _ = sender.send(line);
                }
                // Drain the libtest footer too, so normal exit never sees a closed pipe.
            }
        });
        Self { child, status }
    }

    fn start(&mut self) {
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"start\n")
            .unwrap();
    }
    fn status(&self) -> String {
        self.status.recv_timeout(Duration::from_secs(10)).unwrap()
    }
    fn crash(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn actual_service_and_installer_locks_exclude_each_other_across_processes() {
    let (_scratch, layout) = fixture();
    for kind in ["installer", "service"] {
        let mut child = OwnedChild::spawn(&layout, kind);
        child.start();
        assert_eq!(child.status(), "owned");
        let before = Fingerprint::from(fs::metadata(layout.process_lock()).unwrap());
        assert!(InstallerLock::acquire(&layout).is_err());
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        child.crash();
        let installer = InstallerLock::acquire(&layout).unwrap();
        installer.revalidate().unwrap();
        drop(installer);
        let service = ServiceLock::acquire(layout.clone()).unwrap();
        service.validate_current().unwrap();
        drop(service);
        assert_eq!(
            before,
            Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
        );
    }
}

#[test]
fn competing_process_creation_and_crash_preserve_the_winning_inode() {
    let (_scratch, layout) = fixture();
    let mut children = [
        OwnedChild::spawn(&layout, "installer"),
        OwnedChild::spawn(&layout, "installer"),
    ];
    for child in &mut children {
        child.start();
    }
    let results = [children[0].status(), children[1].status()];
    // Conservative directory revalidation can refuse both contenders; never admit both.
    assert!(results.iter().filter(|status| *status == "owned").count() <= 1);
    assert!(
        results
            .iter()
            .all(|status| status == "owned" || status == "refused")
    );
    let before = Fingerprint::from(fs::metadata(layout.process_lock()).unwrap());
    for (child, status) in children.iter_mut().zip(results) {
        if status == "owned" {
            child.crash();
        } else {
            assert!(child.child.wait().unwrap().success());
        }
    }
    InstallerLock::acquire(&layout)
        .unwrap()
        .revalidate()
        .unwrap();
    assert_eq!(
        before,
        Fingerprint::from(fs::metadata(layout.process_lock()).unwrap())
    );
}
