use super::*;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::process::Command;

pub(crate) struct Fixture {
    pub(crate) scratch: tempfile::TempDir,
    pub(crate) stage: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        // Use the protected checkout, never relax ancestor rules for /tmp.
        let scratch = tempfile::Builder::new()
            .prefix(".release-fixture-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        chmod(scratch.path(), 0o700);
        let stage = fs::canonicalize(scratch.path()).unwrap().join("stage");
        fs::create_dir(&stage).unwrap();
        chmod(&stage, 0o700);
        for name in NAMES {
            let file = stage.join(name.filename());
            fs::write(&file, b"fixture executable bytes").unwrap();
            chmod(&file, 0o500);
        }
        Self { scratch, stage }
    }

    pub(crate) fn file(&self, name: ArtifactName) -> PathBuf {
        self.stage.join(name.filename())
    }
}

pub(crate) fn chmod(path: &Path, mode: u32) {
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
fn anchors_read_only_files_and_repeated_raw_inventory_without_writes() {
    let fixture = Fixture::new();
    let held = AnchoredRelease::open(&fixture.stage).unwrap();
    for (name, file) in NAMES.into_iter().zip(&held.artifacts) {
        assert_eq!(held.path(name), fixture.file(name));
        assert_eq!(file.fingerprint.size, 24);
        let flags = unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        assert_ne!(
            unsafe { libc::fcntl(file.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
    held.revalidate().unwrap();
    held.revalidate().unwrap();
    assert_eq!(fs::read_dir(&fixture.stage).unwrap().count(), 3);
}

#[test]
fn inventory_rejects_missing_extra_hidden_and_non_utf8_entries() {
    let fixture = Fixture::new();
    let file = fixture.file(ArtifactName::Daemon);
    let moved = fixture.scratch.path().join("held");
    fs::rename(&file, &moved).unwrap();
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    fs::rename(&moved, &file).unwrap();
    for name in [OsStr::new("extra"), OsStr::new(".hidden")] {
        let extra = fixture.stage.join(name);
        fs::write(&extra, b"extra").unwrap();
        assert!(AnchoredRelease::open(&fixture.stage).is_err());
        fs::remove_file(extra).unwrap();
    }
    // APFS rejects invalid UTF-8 names at creation; exercise the raw matcher.
    assert_eq!(inventory_index(b"bad\xff"), None);
    assert_eq!(inventory_index(b"handkited\xff"), None);
    AnchoredRelease::open(&fixture.stage).unwrap();
}

#[test]
fn rejects_links_directory_fifo_and_socket_artifacts_without_blocking() {
    let fixture = Fixture::new();
    let file = fixture.file(ArtifactName::Daemon);
    let moved = fixture.scratch.path().join("held");
    fs::rename(&file, &moved).unwrap();
    symlink(&moved, &file).unwrap();
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    fs::remove_file(&file).unwrap();
    fs::hard_link(&moved, &file).unwrap();
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    chmod(&file, 0o500);
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    chmod(&file, 0o700);
    fs::remove_dir(&file).unwrap();
    let path = CString::new(file.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o500) }, 0);
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    fs::remove_file(&file).unwrap();
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("s");
    let _listener = UnixListener::bind(&socket).unwrap();
    fs::rename(socket, &file).unwrap();
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
}

#[test]
fn rejects_wrong_file_modes_and_foreign_owner_metadata() {
    let fixture = Fixture::new();
    let file = fixture.file(ArtifactName::Daemon);
    for mode in [0o400, 0o600, 0o700, 0o755, 0o550, 0o505, 0o4500] {
        chmod(&file, mode);
        assert!(AnchoredRelease::open(&fixture.stage).is_err());
    }
    chmod(&file, 0o500);
    let foreign_uid = unsafe { libc::geteuid() }.wrapping_add(1);
    assert!(artifact_metadata(&File::open(&file).unwrap(), foreign_uid).is_err());
    assert!(directory_metadata(&File::open(&fixture.stage).unwrap(), foreign_uid, true).is_err());
    AnchoredRelease::open(&fixture.stage).unwrap();
}

#[test]
fn rejects_unsafe_parent_stage_and_component_paths() {
    let fixture = Fixture::new();
    for path in [fixture.scratch.path(), &fixture.stage] {
        for mode in [0o755, 0o770, 0o707, 0o1700] {
            chmod(path, mode);
            assert!(AnchoredRelease::open(&fixture.stage).is_err());
        }
        chmod(path, 0o700);
    }
    let moved = fixture.scratch.path().join("moved");
    fs::rename(&fixture.stage, &moved).unwrap();
    symlink(&moved, &fixture.stage).unwrap();
    assert!(AnchoredRelease::open(&fixture.stage).is_err());
    fs::remove_file(&fixture.stage).unwrap();
    fs::rename(&moved, &fixture.stage).unwrap();
    for path in [
        "relative/stage",
        "/",
        "/one",
        "/a/../b",
        "/a/./b",
        "/a//b",
        "/a/b/",
    ] {
        assert!(components(Path::new(path)).is_err());
    }
    let temporary = tempfile::Builder::new().tempdir_in("/private/tmp").unwrap();
    let stage = temporary.path().join("stage");
    fs::create_dir(&stage).unwrap();
    chmod(&stage, 0o700);
    assert!(AnchoredRelease::open(&stage).is_err());
}

#[test]
fn rejects_extended_acls_and_acl_errors_but_allows_deny_only_ancestors() {
    let fixture = Fixture::new();
    for path in [
        fixture.scratch.path(),
        &fixture.stage,
        &fixture.file(ArtifactName::Daemon),
    ] {
        for rule in ["everyone allow read", "everyone deny delete"] {
            acl(path, rule);
            assert!(AnchoredRelease::open(&fixture.stage).is_err());
            clear_acl(path);
        }
    }
    let ancestor = File::open(fixture.scratch.path()).unwrap();
    acl(fixture.scratch.path(), "everyone deny delete");
    check_acl(ancestor.as_raw_fd(), false).unwrap();
    clear_acl(fixture.scratch.path());
    acl(fixture.scratch.path(), "everyone allow read");
    assert!(check_acl(ancestor.as_raw_fd(), false).is_err());
    clear_acl(fixture.scratch.path());
    assert!(check_acl(-1, true).is_err());
    AnchoredRelease::open(&fixture.stage).unwrap();
}

#[test]
fn rejects_file_stage_and_parent_replacement_after_open() {
    for kind in 0..3 {
        let fixture = Fixture::new();
        let held = AnchoredRelease::open(&fixture.stage).unwrap();
        let path = match kind {
            0 => fixture.file(ArtifactName::Daemon),
            1 => fixture.stage.clone(),
            _ => fixture.scratch.path().to_path_buf(),
        };
        let moved = path.with_extension("held");
        fs::rename(&path, &moved).unwrap();
        if kind == 0 {
            fs::write(&path, b"replacement bytes").unwrap();
            chmod(&path, 0o500);
        } else {
            fs::create_dir(&path).unwrap();
            chmod(&path, 0o700);
        }
        assert!(held.revalidate().is_err());
        if kind == 0 {
            fs::remove_file(&path).unwrap();
        } else {
            fs::remove_dir(&path).unwrap();
        }
        fs::rename(&moved, &path).unwrap();
    }
}

#[test]
fn rejects_post_open_metadata_and_inventory_changes() {
    let fixture = Fixture::new();
    let held = AnchoredRelease::open(&fixture.stage).unwrap();
    chmod(&fixture.file(ArtifactName::Daemon), 0o700);
    assert!(held.revalidate().is_err());
    chmod(&fixture.file(ArtifactName::Daemon), 0o500);
    let held = AnchoredRelease::open(&fixture.stage).unwrap();
    fs::write(fixture.stage.join("extra"), b"extra").unwrap();
    assert!(held.revalidate().is_err());
}
