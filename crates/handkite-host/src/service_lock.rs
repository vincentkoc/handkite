//! Lifetime ownership of the single pinned-device service. Lock files are never removed.

use crate::service_layout::ServiceLayout;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub struct ServiceLock {
    layout: ServiceLayout,
    directory: File,
    file: File,
    original: Metadata,
}

impl ServiceLock {
    /// Callers can select neither the lock name nor an alternate service root.
    pub fn acquire(layout: ServiceLayout) -> io::Result<Self> {
        layout.validate_existing_directories()?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(layout.runtime_directory())?;
        let opened = directory.metadata()?;
        layout.validate_existing_directories()?;
        if identity(&opened) != identity(&fs::symlink_metadata(layout.runtime_directory())?) {
            return Err(unsafe_lock());
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"handkite.lock".as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let original = file.metadata()?;
        validate_file(&original, unsafe { libc::geteuid() })?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let owner = Self {
            layout,
            directory,
            file,
            original,
        };
        owner.validate_current()?;
        Ok(owner)
    }

    pub fn layout(&self) -> &ServiceLayout {
        &self.layout
    }

    pub fn validate_current(&self) -> io::Result<()> {
        self.layout.validate_existing_directories()?;
        let directory = self.directory.metadata()?;
        let current_directory = fs::symlink_metadata(self.layout.runtime_directory())?;
        if identity(&directory) != identity(&current_directory) {
            return Err(unsafe_lock());
        }
        let current = fs::symlink_metadata(self.layout.process_lock())?;
        validate_file(&current, unsafe { libc::geteuid() })?;
        if fingerprint(&self.original) != fingerprint(&current)
            || fingerprint(&self.original) != fingerprint(&self.file.metadata()?)
        {
            return Err(unsafe_lock());
        }
        Ok(())
    }
}

fn identity(metadata: &Metadata) -> (u64, u64, u32, u32, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.gid(),
        metadata.mode(),
    )
}

fn fingerprint(metadata: &Metadata) -> impl PartialEq + use<> {
    (
        identity(metadata),
        metadata.nlink(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn validate_file(metadata: &Metadata, uid: u32) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(unsafe_lock());
    }
    Ok(())
}

fn unsafe_lock() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe or replaced service lock",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_layout::tests::fixture;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Stdio};

    #[test]
    fn process_owner_excludes_competitors_and_crash_releases_lock() {
        const CHILD: &str = "HANDKITE_TEST_LOCK_FILE";
        if let Some(path) = std::env::var_os(CHILD) {
            // The child competes through the OS, without a production path override.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            assert_eq!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            println!("owned");
            std::io::stdout().flush().unwrap();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            return;
        }
        let (_scratch, layout) = fixture();
        let owner = ServiceLock::acquire(layout).unwrap();
        let path = owner.layout().process_lock();
        let original = fs::metadata(&path).unwrap();
        assert_eq!(original.mode() & 0o7777, 0o600);
        assert!(ServiceLock::acquire(owner.layout.clone()).is_err());
        // Release normally, then let a test-owned process take the same inode.
        let layout = owner.layout.clone();
        drop(owner);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service_lock::tests::process_owner_excludes_competitors_and_crash_releases_lock",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if line.trim() == "owned" {
                break;
            }
            line.clear();
        }
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        let replacement = ServiceLock::acquire(layout).unwrap();
        assert_eq!(identity(&fs::metadata(path).unwrap()), identity(&original));
        replacement.validate_current().unwrap();
    }

    #[test]
    fn unsafe_files_and_replaced_lock_fail_without_changing_marker() {
        let (_scratch, layout) = fixture();
        let path = layout.process_lock();
        let marker = layout.state_directory().join("UNLOCK-DISABLED");
        fs::write(&marker, b"existing-stop\n").unwrap();
        let marker_before = fs::metadata(&marker).unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&marker, &path).unwrap();
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = path.with_extension("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(ServiceLock::acquire(layout.clone()).is_err());
        fs::remove_file(alias).unwrap();
        let owner = ServiceLock::acquire(layout).unwrap();
        assert!(
            validate_file(
                &owner.file.metadata().unwrap(),
                unsafe { libc::geteuid() }.wrapping_add(1)
            )
            .is_err()
        );
        let held = path.with_extension("held");
        fs::rename(&path, &held).unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(owner.validate_current().is_err());
        assert_eq!(fs::read(&marker).unwrap(), b"existing-stop\n");
        assert!(fingerprint(&fs::metadata(marker).unwrap()) == fingerprint(&marker_before));
    }

    #[test]
    fn replaced_or_unprotected_runtime_invalidates_owner() {
        let (_scratch, layout) = fixture();
        let runtime = layout.runtime_directory();
        let owner = ServiceLock::acquire(layout).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(owner.validate_current().is_err());
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        owner.validate_current().unwrap();
        let moved = runtime.with_extension("held");
        fs::rename(&runtime, &moved).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(owner.validate_current().is_err());
    }
}
