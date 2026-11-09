use super::*;
use crate::release_files::{components, directory_matches, directory_metadata, open_at};

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    private: bool,
    account_owned: bool,
}

pub(super) struct Releases {
    components: Vec<CString>,
    directories: Vec<Directory>,
    uid: u32,
}

impl Releases {
    pub(super) fn open(owner: &InstallerLock) -> Result<Self, &'static str> {
        let components = components(&owner.layout().releases_directory())?;
        let home_index = components.len().checked_sub(4).ok_or(UNSAFE)?;
        let uid = unsafe { libc::geteuid() };
        let mut directories = Vec::with_capacity(components.len() + 1);
        let mut file = open_at(libc::AT_FDCWD, c"/", true)?;
        for index in 0..=components.len() {
            let private = index == components.len();
            let account_owned = index >= home_index;
            let fingerprint = directory_policy(&file, uid, private, account_owned)?;
            directories.push(Directory {
                file,
                fingerprint,
                private,
                account_owned,
            });
            if let Some(component) = components.get(index) {
                file = open_at(
                    directories.last().ok_or(UNSAFE)?.file.as_raw_fd(),
                    component,
                    true,
                )?;
            } else {
                break;
            }
        }
        let value = Self {
            components,
            directories,
            uid,
        };
        value.revalidate(false)?;
        Ok(value)
    }

    pub(super) fn directory(&self) -> &File {
        &self
            .directories
            .last()
            .expect("root directory is held")
            .file
    }

    fn revalidate(&self, created: bool) -> Result<File, &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            for file in [&held.file, &walked] {
                let current = directory_policy(file, self.uid, held.private, held.account_owned)?;
                if !directory_matches(&current, &held.fingerprint, held.private && !created) {
                    return Err(UNSAFE);
                }
            }
        }
        Ok(walked)
    }

    pub(super) fn before_create(&self) -> Result<(), &'static str> {
        self.revalidate(false).map(|_| ())
    }

    pub(super) fn after_create(&mut self) -> Result<(), &'static str> {
        let fresh = self.revalidate(true)?;
        let held = self.directories.last_mut().ok_or(UNSAFE)?;
        let fingerprint = directory_policy(&held.file, self.uid, true, true)?;
        if directory_policy(&fresh, self.uid, true, true)? != fingerprint {
            return Err(UNSAFE);
        }
        held.fingerprint = fingerprint;
        self.before_create()
    }
}

fn directory_policy(
    file: &File,
    uid: u32,
    private: bool,
    account_owned: bool,
) -> Result<Fingerprint, &'static str> {
    let fingerprint = directory_metadata(file, uid, private)?;
    if account_owned && file.metadata().map_err(|_| UNSAFE)?.uid() != uid {
        return Err(UNSAFE);
    }
    Ok(fingerprint)
}

pub(super) struct CreatedStage {
    pub(super) releases: Releases,
    pub(super) file: File,
    pub(super) path: std::path::PathBuf,
    name: CString,
    fingerprint: Fingerprint,
}

impl CreatedStage {
    pub(super) fn open(
        releases: Releases,
        owner: &InstallerLock,
        name: &str,
    ) -> Result<Self, &'static str> {
        let name_c = CString::new(name).map_err(|_| UNSAFE)?;
        let file = open_at(releases.directory().as_raw_fd(), &name_c, true)?;
        let fingerprint = directory_policy(&file, releases.uid, true, true)?;
        let value = Self {
            releases,
            file,
            path: owner.layout().releases_directory().join(name),
            name: name_c,
            fingerprint,
        };
        value.revalidate(false)?;
        require_empty(&value.file)?;
        Ok(value)
    }

    pub(super) fn revalidate(&self, file_created: bool) -> Result<File, &'static str> {
        let parent = self.releases.revalidate(false)?;
        let fresh = open_at(parent.as_raw_fd(), &self.name, true)?;
        for file in [&self.file, &fresh] {
            let current = directory_policy(file, self.releases.uid, true, true)?;
            if !directory_matches(&current, &self.fingerprint, !file_created) {
                return Err(UNSAFE);
            }
        }
        Ok(fresh)
    }

    pub(super) fn after_file_created(&mut self) -> Result<(), &'static str> {
        let fresh = self.revalidate(true)?;
        let fingerprint = directory_policy(&self.file, self.releases.uid, true, true)?;
        if directory_policy(&fresh, self.releases.uid, true, true)? != fingerprint {
            return Err(UNSAFE);
        }
        self.fingerprint = fingerprint;
        self.revalidate(false).map(|_| ())
    }
}

fn require_empty(directory: &File) -> Result<(), &'static str> {
    let scan = open_at(directory.as_raw_fd(), c".", true)?;
    let raw = unsafe { libc::fdopendir(scan.as_raw_fd()) };
    if raw.is_null() {
        return Err(UNSAFE);
    }
    std::mem::forget(scan);
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let scan = Dir(raw);
    loop {
        unsafe { *libc::__error() = 0 };
        let entry = unsafe { libc::readdir(scan.0) };
        if entry.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(UNSAFE)
            };
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            return Err(UNSAFE);
        }
    }
}
