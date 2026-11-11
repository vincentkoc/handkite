use super::*;
use crate::release_files::{components, directory_matches, directory_metadata};
use std::ffi::{CStr, CString};

struct Directory {
    file: File,
    fingerprint: Fingerprint,
}

pub(super) struct Anchor {
    components: Vec<CString>,
    directories: Vec<Directory>,
    private: bool,
}

impl Anchor {
    pub(super) fn open(path: &Path, private: bool) -> Result<Self, &'static str> {
        let components = components(path)?;
        let mut directories = Vec::new();
        let mut file = open_at(libc::AT_FDCWD, c"/", true)?;
        for index in 0..=components.len() {
            let leaf = index == components.len();
            let fingerprint = directory_policy(&file, leaf, leaf && private)?;
            directories.push(Directory { file, fingerprint });
            if let Some(name) = components.get(index) {
                file = open_at(directories[index].file.as_raw_fd(), name, true)?;
            } else {
                break;
            }
        }
        let value = Self {
            components,
            directories,
            private,
        };
        value.check(false)?;
        Ok(value)
    }

    pub(super) fn file(&self) -> &File {
        &self.directories.last().expect("anchored root").file
    }

    /// Only the leaf's content metadata may change at a caller-owned mutation.
    pub(super) fn check(&self, leaf_changed: bool) -> Result<(), &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            let leaf = index == self.components.len();
            let current = directory_policy(&walked, leaf, leaf && self.private)?;
            let original = directory_policy(&held.file, leaf, leaf && self.private)?;
            if (leaf && current != original)
                || !directory_matches(&current, &held.fingerprint, leaf && !leaf_changed)
                || !directory_matches(&original, &held.fingerprint, leaf && !leaf_changed)
            {
                return Err(UNSAFE);
            }
        }
        Ok(())
    }

    pub(super) fn refresh(&mut self) -> Result<(), &'static str> {
        self.check(true)?;
        let fingerprint = directory_policy(self.file(), true, self.private)?;
        self.directories.last_mut().ok_or(UNSAFE)?.fingerprint = fingerprint;
        self.check(false)
    }
}

fn directory_policy(file: &File, leaf: bool, private: bool) -> Result<Fingerprint, &'static str> {
    let uid = unsafe { libc::geteuid() };
    let fingerprint = directory_metadata(file, uid, private)?;
    if leaf {
        if file.metadata().map_err(|_| UNSAFE)?.uid() != uid {
            return Err(UNSAFE);
        }
        check_acl(file.as_raw_fd(), true)?;
    }
    Ok(fingerprint)
}

pub(super) struct HeldFile {
    pub(super) file: File,
    pub(super) fingerprint: Fingerprint,
    mode: u32,
}

impl HeldFile {
    pub(super) fn open(
        parent: &File,
        name: &CStr,
        mode: Option<u32>,
    ) -> Result<Self, &'static str> {
        let file = open_at(parent.as_raw_fd(), name, false)?;
        let actual_mode = file.metadata().map_err(|_| UNSAFE)?.mode() & 0o7777;
        if mode.is_none() && ![0o500, 0o700, 0o755].contains(&actual_mode) {
            return Err(UNSAFE);
        }
        let mode = mode.unwrap_or(actual_mode);
        if ![0o500, 0o700, 0o755].contains(&mode) && mode != 0o600 {
            return Err(UNSAFE);
        }
        let fingerprint = file_policy(&file, mode)?;
        Ok(Self {
            file,
            fingerprint,
            mode,
        })
    }

    pub(super) fn check(&self, parent: &File, name: &CStr) -> Result<(), &'static str> {
        let current = Self::open(parent, name, Some(self.mode))?;
        if current.fingerprint != self.fingerprint
            || file_policy(&self.file, self.mode)? != self.fingerprint
        {
            return Err(UNSAFE);
        }
        Ok(())
    }

    pub(super) fn freeze(&mut self, mode: u32) -> Result<(), &'static str> {
        if file_policy(&self.file, self.mode)? != self.fingerprint
            || unsafe { libc::fchmod(self.file.as_raw_fd(), mode.try_into().map_err(|_| UNSAFE)?) }
                != 0
        {
            return Err(UNSAFE);
        }
        self.mode = mode;
        self.fingerprint = file_policy(&self.file, mode)?;
        self.file.sync_all().map_err(|_| UNSAFE)
    }

    pub(super) fn check_replaced(&self) -> Result<(), &'static str> {
        let current: Fingerprint = self.file.metadata().map_err(|_| UNSAFE)?.into();
        if !current.matches_after_unlink(&self.fingerprint) {
            return Err(UNSAFE);
        }
        check_acl(self.file.as_raw_fd(), true)
    }
}

pub(super) fn file_policy(file: &File, mode: u32) -> Result<Fingerprint, &'static str> {
    let meta = file.metadata().map_err(|_| UNSAFE)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != mode
        || meta.nlink() != 1
        || meta.len() > MAX_ARTIFACT_BYTES
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(meta.into())
}

pub(super) fn create_file(parent: &File, name: &CStr) -> Result<File, &'static str> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(UNSAFE);
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if file_policy(&file, 0o600)?.size != 0 {
        return Err(UNSAFE);
    }
    Ok(file)
}

pub(super) fn mkdir(parent: &File, name: &CStr) -> Result<(), &'static str> {
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(UNSAFE);
    }
    Ok(())
}

pub(super) fn identity(file: &File) -> Result<(u64, u64), &'static str> {
    let meta = file.metadata().map_err(|_| UNSAFE)?;
    Ok((meta.dev(), meta.ino()))
}

pub(super) fn inventory(directory: &File, expected: &[&str]) -> Result<(), &'static str> {
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
    let mut seen = vec![false; expected.len()];
    loop {
        unsafe { *libc::__error() = 0 };
        let entry = unsafe { libc::readdir(scan.0) };
        if entry.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(0)
                && seen.iter().all(|value| *value)
            {
                Ok(())
            } else {
                Err(UNSAFE)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let index = expected
            .iter()
            .position(|item| item.as_bytes() == name)
            .ok_or(UNSAFE)?;
        if seen[index] {
            return Err(UNSAFE);
        }
        seen[index] = true;
    }
}
