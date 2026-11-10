//! Staged service paths. This module never installs, creates, or repairs state.

use crate::account_home::account_home;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Fixed paths beneath the effective OS account home; callers cannot supply a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceLayout {
    home: PathBuf,
}

impl ServiceLayout {
    /// Derive paths only. This does not require or create an installed service.
    pub fn current_account() -> io::Result<Self> {
        let home = account_home()
            .ok_or_else(|| io::Error::other("effective account home is unavailable"))?;
        Ok(Self { home })
    }

    pub fn root(&self) -> PathBuf {
        self.home.join("Library/Application Support/Handkite")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root().join("config/host.json")
    }

    pub fn releases_directory(&self) -> PathBuf {
        self.root().join("releases")
    }

    /// Fixed relative-symlink location; no selection is read, created, or changed.
    pub fn current_release_selection(&self) -> PathBuf {
        self.releases_directory().join("current")
    }

    /// Fixed executable path through the future release-selection protocol.
    pub fn current_release_executable(&self) -> PathBuf {
        self.current_release_selection().join("handkited")
    }

    /// Fixed bridge executable; no release selection is read or changed.
    pub fn current_release_mcp_executable(&self) -> PathBuf {
        self.current_release_selection().join("handkite-mcp")
    }

    pub fn runtime_directory(&self) -> PathBuf {
        self.root().join("runtime")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_directory().join("handkite.sock")
    }

    pub fn process_lock(&self) -> PathBuf {
        self.runtime_directory().join("handkite.lock")
    }

    pub fn state_directory(&self) -> PathBuf {
        self.root().join("state")
    }

    /// A staging path only, outside launchd's automatically loaded directories.
    pub fn staged_launchd_descriptor(&self) -> PathBuf {
        self.root().join("launchd/com.handkite.host.plist")
    }

    /// Inspect existing directory metadata without opening config, releases, or state files.
    ///
    /// This is a point-in-time preflight, not a race-safe installation capability.
    /// A future installer must revalidate through anchored directory descriptors.
    pub fn validate_existing_directories(&self) -> io::Result<()> {
        if !self.home.is_absolute() || fs::canonicalize(&self.home)? != self.home {
            return Err(unsafe_directory());
        }
        let uid = unsafe { libc::geteuid() };
        for ancestor in self.home.ancestors().skip(1) {
            validate_directory(ancestor, uid, true, false)?;
        }
        for (directory, private) in self.directories() {
            validate_directory(&directory, uid, false, private)?;
        }
        Ok(())
    }

    fn directories(&self) -> [(PathBuf, bool); 9] {
        [
            (self.home.clone(), false),
            (self.home.join("Library"), false),
            (self.home.join("Library/Application Support"), false),
            (self.root(), false),
            (self.state_directory(), false),
            (self.root().join("config"), true),
            (self.releases_directory(), true),
            (self.runtime_directory(), true),
            (self.root().join("launchd"), true),
        ]
    }
}

fn validate_directory(path: &Path, uid: u32, allow_root: bool, private: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.permissions().mode();
    if !metadata.file_type().is_dir()
        || (metadata.uid() != uid && !(allow_root && metadata.uid() == 0))
        || mode & 0o022 != 0
        || (private && mode & 0o7777 != 0o700)
    {
        return Err(unsafe_directory());
    }
    Ok(())
}

fn unsafe_directory() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe service directory")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    pub(crate) fn layout_at(home: impl Into<PathBuf>) -> ServiceLayout {
        ServiceLayout { home: home.into() }
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, ServiceLayout) {
        fixture_with(tempfile::tempdir().unwrap())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn fixture_in(parent: &Path) -> (tempfile::TempDir, ServiceLayout) {
        fixture_with(tempfile::tempdir_in(parent).unwrap())
    }

    fn fixture_with(scratch: tempfile::TempDir) -> (tempfile::TempDir, ServiceLayout) {
        let layout = ServiceLayout {
            home: fs::canonicalize(scratch.path()).unwrap().join("account"),
        };
        for (directory, _) in layout.directories() {
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (scratch, layout)
    }

    pub(crate) fn socket_fixture() -> (tempfile::TempDir, ServiceLayout) {
        // Darwin limits Unix socket paths to 103 bytes plus the terminator.
        let scratch = tempfile::Builder::new()
            .prefix("h")
            .rand_bytes(3)
            .tempdir()
            .unwrap();
        let layout = ServiceLayout {
            home: fs::canonicalize(scratch.path()).unwrap(),
        };
        for (directory, _) in layout.directories().into_iter().skip(1) {
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (scratch, layout)
    }

    #[test]
    fn fixed_paths_ignore_home_and_working_directory() {
        const EXPECTED: &str = "HANDKITE_TEST_SERVICE_HOME";
        if let Some(expected) = std::env::var_os(EXPECTED) {
            let layout = ServiceLayout::current_account().unwrap();
            assert_eq!(layout.home, PathBuf::from(expected));
            let root = layout.home.join("Library/Application Support/Handkite");
            assert_eq!(layout.root(), root);
            assert_eq!(layout.config_file(), root.join("config/host.json"));
            assert_eq!(layout.releases_directory(), root.join("releases"));
            assert_eq!(
                layout.current_release_selection(),
                root.join("releases/current")
            );
            assert_eq!(
                layout.current_release_executable(),
                root.join("releases/current/handkited")
            );
            assert_eq!(
                layout.current_release_mcp_executable(),
                root.join("releases/current/handkite-mcp")
            );
            assert_eq!(layout.runtime_directory(), root.join("runtime"));
            assert_eq!(layout.socket(), root.join("runtime/handkite.sock"));
            assert_eq!(layout.process_lock(), root.join("runtime/handkite.lock"));
            assert_eq!(layout.state_directory(), root.join("state"));
            assert_eq!(
                layout.staged_launchd_descriptor(),
                root.join("launchd/com.handkite.host.plist")
            );
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service_layout::tests::fixed_paths_ignore_home_and_working_directory",
            ])
            .env(EXPECTED, account_home().unwrap())
            .env("HOME", scratch.path())
            .current_dir(scratch.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn validation_preserves_existing_marker_and_creates_no_service_files() {
        let (_scratch, layout) = fixture();
        let state = layout.state_directory().join("iphone");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = state.join("UNLOCK-DISABLED");
        fs::write(&marker, b"existing-stop\n").unwrap();
        let original = fs::metadata(&marker).unwrap();
        for _ in 0..2 {
            layout.validate_existing_directories().unwrap();
        }
        assert_eq!(fs::read(&marker).unwrap(), b"existing-stop\n");
        let after = fs::metadata(&marker).unwrap();
        assert_eq!((after.dev(), after.ino()), (original.dev(), original.ino()));
        for path in [
            layout.config_file(),
            layout.socket(),
            layout.staged_launchd_descriptor(),
        ] {
            assert!(!path.exists());
        }
        assert_eq!(
            fs::read_dir(layout.releases_directory()).unwrap().count(),
            0
        );
    }

    #[test]
    fn missing_symlinked_writable_or_foreign_directories_fail_closed() {
        let (_scratch, layout) = fixture();
        for (directory, private) in layout.directories() {
            let moved = directory.with_extension("held");
            fs::rename(&directory, &moved).unwrap();
            assert!(layout.validate_existing_directories().is_err());
            assert!(!directory.exists());
            symlink(&moved, &directory).unwrap();
            assert!(layout.validate_existing_directories().is_err());
            fs::remove_file(&directory).unwrap();
            fs::write(&directory, b"not a directory").unwrap();
            assert!(layout.validate_existing_directories().is_err());
            fs::remove_file(&directory).unwrap();
            fs::rename(moved, &directory).unwrap();
            for mode in [0o770, 0o707] {
                fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
                assert!(layout.validate_existing_directories().is_err());
            }
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let foreign_uid = unsafe { libc::geteuid() }.wrapping_add(1);
            assert!(validate_directory(&directory, foreign_uid, false, private).is_err());
        }
        layout.validate_existing_directories().unwrap();
    }

    #[test]
    fn service_directories_are_private_and_home_ancestors_are_protected() {
        let (scratch, layout) = fixture();
        for (directory, private) in layout.directories() {
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(layout.validate_existing_directories().is_err(), private);
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(layout.validate_existing_directories().is_err());
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o700)).unwrap();
        layout.validate_existing_directories().unwrap();
    }
}
