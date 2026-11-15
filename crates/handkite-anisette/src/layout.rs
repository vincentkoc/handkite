//! Fixed account paths for Mac-side Anisette state. Never creates or repairs.
//!
//! The paths match the Handkite host service layout. They are derived here so
//! this optional crate does not depend on `handkite-host`.

use std::ffi::{CStr, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Fixed paths beneath the effective OS account home; callers cannot supply a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateLayout {
    home: PathBuf,
}

impl StateLayout {
    pub(crate) fn current_account() -> io::Result<Self> {
        let home = account_home()
            .ok_or_else(|| io::Error::other("effective account home is unavailable"))?;
        Ok(Self { home })
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.home.join("Library/Application Support/Handkite")
    }

    pub(crate) fn state_directory(&self) -> PathBuf {
        self.root().join("state")
    }

    /// Fixed Mac-side V3 reservation and opaque provisioning state.
    #[cfg(test)]
    pub(crate) fn anisette_state_directory(&self) -> PathBuf {
        self.state_directory().join("anisette-v3")
    }
}

/// Resolve the effective account, without consulting caller-controlled environment paths.
fn account_home() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0_u8; 1024];
    loop {
        let mut record = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // The OS record points into this buffer, which stays alive until the path is copied.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return None;
        }
        let home = PathBuf::from(OsStr::from_bytes(unsafe {
            CStr::from_ptr(directory).to_bytes()
        }));
        return home.is_absolute().then_some(home);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// A private account tree containing only the directories state custody walks.
    pub(crate) fn fixture() -> (tempfile::TempDir, StateLayout) {
        let scratch = tempfile::tempdir().unwrap();
        let layout = StateLayout {
            home: fs::canonicalize(scratch.path()).unwrap().join("account"),
        };
        for directory in [
            layout.home.clone(),
            layout.home.join("Library"),
            layout.home.join("Library/Application Support"),
            layout.root(),
            layout.state_directory(),
        ] {
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (scratch, layout)
    }

    #[test]
    fn fixed_paths_match_host_service_layout() {
        let layout = StateLayout {
            home: PathBuf::from("/Users/fixture"),
        };
        assert_eq!(
            layout.anisette_state_directory(),
            PathBuf::from("/Users/fixture/Library/Application Support/Handkite/state/anisette-v3")
        );
    }
}
