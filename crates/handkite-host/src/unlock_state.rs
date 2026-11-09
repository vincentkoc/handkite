use crate::account_home::account_home;
use crate::file_gate::FileAttemptGate;
use handkite_core::unlock::GateError;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const STATE_COMPONENTS: [&str; 5] = [
    "Library",
    "Application Support",
    "Handkite",
    "state",
    "iphone",
];

/// Open the one-phone stop gate without creating or relocating account state.
pub fn iphone_gate() -> Result<FileAttemptGate, GateError> {
    gate_for_home(&account_home().ok_or(GateError)?)
}

fn gate_for_home(home: &Path) -> Result<FileAttemptGate, GateError> {
    // Reject symlinks and noncanonical components even above the account home.
    if !home.is_absolute() || fs::canonicalize(home).map_err(|_| GateError)? != home {
        return Err(GateError);
    }
    let uid = unsafe { libc::geteuid() };
    validate_directory(home, uid)?;
    let mut directory = home.to_path_buf();
    for component in STATE_COMPONENTS {
        directory.push(component);
        validate_directory(&directory, uid)?;
    }
    FileAttemptGate::new(directory.join("UNLOCK-DISABLED"))
}

fn validate_directory(path: &Path, uid: u32) -> Result<(), GateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| GateError)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(GateError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use handkite_core::unlock::AttemptGate;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct AccountHome(PathBuf);

    impl AccountHome {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "handkite-account-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn provision(&self) -> PathBuf {
            let mut directory = self.0.clone();
            for component in STATE_COMPONENTS {
                directory.push(component);
                fs::create_dir(&directory).unwrap();
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
            directory.join("UNLOCK-DISABLED")
        }
    }

    impl Drop for AccountHome {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn existing_marker_remains_disabled_at_the_original_path() {
        let home = AccountHome::new();
        let marker = home.provision();
        fs::write(&marker, b"existing-stop\n").unwrap();
        let original = fs::metadata(&marker).unwrap();
        for _ in 0..2 {
            let mut gate = gate_for_home(&home.0).unwrap();
            assert!(gate.is_disabled().unwrap());
            assert!(gate.clear().is_err());
            assert!(gate.mark_uncertain().is_err());
        }
        assert_eq!(fs::read(&marker).unwrap(), b"existing-stop\n");
        assert_eq!(fs::metadata(marker).unwrap().ino(), original.ino());
    }

    #[test]
    fn missing_state_is_rejected_without_creating_directories() {
        let home = AccountHome::new();
        assert!(gate_for_home(&home.0).is_err());
        assert!(!home.0.join("Library").exists());
        let marker = home.provision();
        fs::remove_dir(marker.parent().unwrap()).unwrap();
        assert!(gate_for_home(&home.0).is_err());
        assert!(!marker.parent().unwrap().exists());
    }

    #[test]
    fn writable_or_symlinked_ancestors_and_nonprivate_state_are_rejected() {
        let home = AccountHome::new();
        let marker = home.provision();
        let mut directories = vec![home.0.clone()];
        for component in STATE_COMPONENTS {
            directories.push(directories.last().unwrap().join(component));
        }
        for directory in &directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
            assert!(gate_for_home(&home.0).is_err());
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            let moved = directory.with_extension("real");
            fs::rename(directory, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, directory).unwrap();
            assert!(gate_for_home(&home.0).is_err());
            fs::remove_file(directory).unwrap();
            fs::rename(moved, directory).unwrap();
        }
        fs::set_permissions(marker.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(gate_for_home(&home.0).is_err());
    }

    #[test]
    fn nonwritable_readable_ancestors_are_accepted_but_foreign_owners_are_not() {
        let home = AccountHome::new();
        home.provision();
        fs::set_permissions(&home.0, fs::Permissions::from_mode(0o750)).unwrap();
        let handkite = home.0.join("Library/Application Support/Handkite");
        for path in [&handkite, &handkite.join("state")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(!gate_for_home(&home.0).unwrap().is_disabled().unwrap());
        let other_uid = unsafe { libc::geteuid() }.wrapping_add(1);
        assert!(validate_directory(&home.0, other_uid).is_err());
    }

    #[test]
    fn account_home_ignores_home_environment_and_working_directory() {
        const EXPECTED: &str = "HANDKITE_TEST_ACCOUNT_HOME";
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert_eq!(account_home().unwrap(), PathBuf::from(expected));
            return;
        }
        let home = AccountHome::new();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unlock_state::tests::account_home_ignores_home_environment_and_working_directory",
            ])
            .env(EXPECTED, account_home().unwrap())
            .env("HOME", &home.0)
            .current_dir(&home.0)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
