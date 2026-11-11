use crate::release_files::{
    Fingerprint, check_acl, components, directory_matches, directory_metadata, open_at,
};
use crate::service_layout::ServiceLayout;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};

pub(super) const UNSAFE: &str = "unsafe or changed service descriptor";
pub(super) const FINAL: &CStr = c"com.handkite.host.plist";

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    account_owned: bool,
    private: bool,
}

pub(super) struct Anchors {
    components: Vec<CString>,
    directories: Vec<Directory>,
    uid: u32,
}

impl Anchors {
    pub(super) fn open(layout: &ServiceLayout) -> Result<Self, &'static str> {
        let mut parts = components(&layout.staged_launchd_descriptor())?;
        if parts.pop().as_deref() != Some(FINAL) {
            return Err(UNSAFE);
        }
        let home_index = parts.len().checked_sub(4).ok_or(UNSAFE)?;
        let uid = unsafe { libc::geteuid() };
        let mut directories = Vec::with_capacity(parts.len() + 1);
        let mut file = open_at(libc::AT_FDCWD, c"/", true)?;
        for index in 0..=parts.len() {
            let private = index == parts.len();
            let account_owned = index >= home_index;
            let fingerprint = policy(&file, uid, private, account_owned)?;
            directories.push(Directory {
                file,
                fingerprint,
                private,
                account_owned,
            });
            if let Some(component) = parts.get(index) {
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
            components: parts,
            directories,
            uid,
        };
        value.check(&[])?;
        Ok(value)
    }

    pub(super) fn parent(&self) -> &File {
        &self.directories.last().unwrap().file
    }

    fn walk(&self, transition: Option<bool>) -> Result<Fingerprint, &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        let mut leaf = None;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            for file in [&held.file, &walked] {
                let current = policy(file, self.uid, held.private, held.account_owned)?;
                let matches = if let Some(added_file) = transition.filter(|_| held.private) {
                    current.matches_after_entry_change(&held.fingerprint, added_file)
                } else {
                    directory_matches(&current, &held.fingerprint, held.private)
                };
                if !matches {
                    return Err(UNSAFE);
                }
                if held.private {
                    if leaf.as_ref().is_some_and(|old| old != &current) {
                        return Err(UNSAFE);
                    }
                    leaf = Some(current);
                }
            }
        }
        leaf.ok_or(UNSAFE)
    }

    pub(super) fn check(&self, expected: &[&CStr]) -> Result<(), &'static str> {
        self.walk(None)?;
        inventory(self.parent(), expected)?;
        self.walk(None)?;
        Ok(())
    }

    /// Accept only the described entry transition, retaining every anchor identity.
    pub(super) fn transition(
        &mut self,
        expected: &[&CStr],
        added_file: bool,
    ) -> Result<(), &'static str> {
        let observed = self.walk(Some(added_file))?;
        inventory(self.parent(), expected)?;
        if self.walk(Some(added_file))? != observed {
            return Err(UNSAFE);
        }
        self.directories.last_mut().ok_or(UNSAFE)?.fingerprint = observed;
        self.check(expected)
    }
}

fn policy(
    file: &File,
    uid: u32,
    private: bool,
    account_owned: bool,
) -> Result<Fingerprint, &'static str> {
    let result = directory_metadata(file, uid, private)?;
    if account_owned && file.metadata().map_err(|_| UNSAFE)?.uid() != uid {
        return Err(UNSAFE);
    }
    Ok(result)
}

pub(super) fn file_policy(
    file: &File,
    mode: u32,
    size: usize,
) -> Result<Fingerprint, &'static str> {
    let meta = file.metadata().map_err(|_| UNSAFE)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o7777 != mode
        || meta.len() != size as u64
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(meta.into())
}

pub(super) fn bytes_match(file: &File, bytes: &[u8]) -> Result<(), &'static str> {
    let mut read = vec![0; bytes.len() + 1];
    let mut offset = 0;
    while offset < read.len() {
        let count = file
            .read_at(&mut read[offset..], offset as u64)
            .map_err(|_| UNSAFE)?;
        if count == 0 {
            break;
        }
        offset += count;
    }
    if &read[..offset] != bytes {
        return Err(UNSAFE);
    }
    Ok(())
}

pub(super) struct Descriptor {
    pub(super) file: File,
    pub(super) fingerprint: Fingerprint,
    pub(super) bytes: Box<[u8]>,
}

impl Descriptor {
    pub(super) fn check(&self, anchors: &Anchors, name: &CStr) -> Result<(), &'static str> {
        let current = open_at(anchors.parent().as_raw_fd(), name, false)?;
        for file in [&self.file, &current] {
            if file_policy(file, 0o400, self.bytes.len())? != self.fingerprint {
                return Err(UNSAFE);
            }
            bytes_match(file, &self.bytes)?;
            if file_policy(file, 0o400, self.bytes.len())? != self.fingerprint {
                return Err(UNSAFE);
            }
        }
        Ok(())
    }

    pub(super) fn renamed(&mut self, anchors: &Anchors) -> Result<(), &'static str> {
        let current = open_at(anchors.parent().as_raw_fd(), FINAL, false)?;
        let next = file_policy(&current, 0o400, self.bytes.len())?;
        if !next.matches_after_rename(&self.fingerprint)
            || file_policy(&self.file, 0o400, self.bytes.len())? != next
        {
            return Err(UNSAFE);
        }
        bytes_match(&self.file, &self.bytes)?;
        bytes_match(&current, &self.bytes)?;
        self.fingerprint = next;
        self.check(anchors, FINAL)
    }
}

fn inventory(directory: &File, expected: &[&CStr]) -> Result<(), &'static str> {
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
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name == c"." || name == c".." {
            continue;
        }
        let index = expected
            .iter()
            .position(|value| *value == name)
            .ok_or(UNSAFE)?;
        if seen[index] {
            return Err(UNSAFE);
        }
        seen[index] = true;
    }
}
