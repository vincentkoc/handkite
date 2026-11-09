use handkite_core::unlock::{AttemptGate, GateError};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A crash-persistent block on further PIN attempts. It never stores the PIN.
pub struct FileAttemptGate {
    path: PathBuf,
    owned_inode: Option<(u64, u64)>,
}

impl FileAttemptGate {
    pub fn new(path: PathBuf) -> Result<Self, GateError> {
        if !path.is_absolute() {
            return Err(GateError);
        }
        let parent = path.parent().ok_or(GateError)?;
        let meta = fs::symlink_metadata(parent).map_err(|_| GateError)?;
        if !meta.file_type().is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(GateError);
        }
        Ok(Self {
            path,
            owned_inode: None,
        })
    }

    fn parent(&self) -> &Path {
        self.path.parent().expect("validated absolute path")
    }
}

impl AttemptGate for FileAttemptGate {
    fn is_disabled(&self) -> Result<bool, GateError> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(_) => Err(GateError),
        }
    }

    fn mark_uncertain(&mut self) -> Result<(), GateError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|_| GateError)?;
        file.write_all(b"uncertain\n").map_err(|_| GateError)?;
        file.sync_all().map_err(|_| GateError)?;
        let meta = file.metadata().map_err(|_| GateError)?;
        File::open(self.parent())
            .and_then(|dir| dir.sync_all())
            .map_err(|_| GateError)?;
        self.owned_inode = Some((meta.dev(), meta.ino()));
        Ok(())
    }

    fn clear(&mut self) -> Result<(), GateError> {
        let owned = self.owned_inode.ok_or(GateError)?;
        let meta = fs::symlink_metadata(&self.path).map_err(|_| GateError)?;
        if !meta.file_type().is_file() || (meta.dev(), meta.ino()) != owned {
            return Err(GateError);
        }
        fs::remove_file(&self.path).map_err(|_| GateError)?;
        self.owned_inode = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct PrivateDir(PathBuf);

    impl PrivateDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "handkite-gate-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn marker(&self) -> PathBuf {
            self.0.join("UNLOCK-DISABLED")
        }
    }

    impl Drop for PrivateDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn marker_is_private_and_survives_a_process_replacement() {
        let dir = PrivateDir::new();
        let path = dir.marker();
        let mut first = FileAttemptGate::new(path.clone()).unwrap();
        first.mark_uncertain().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let mut replacement = FileAttemptGate::new(path.clone()).unwrap();
        assert!(replacement.is_disabled().unwrap());
        assert!(replacement.mark_uncertain().is_err());
        assert!(replacement.clear().is_err());

        first.clear().unwrap();
        assert!(!replacement.is_disabled().unwrap());
    }

    #[test]
    fn rejects_an_insecure_parent_or_existing_symlink() {
        let dir = PrivateDir::new();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FileAttemptGate::new(dir.marker()).is_err());
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();

        let target = dir.0.join("target");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.marker()).unwrap();
        let mut gate = FileAttemptGate::new(dir.marker()).unwrap();
        assert!(gate.is_disabled().unwrap());
        assert!(gate.mark_uncertain().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }
}
