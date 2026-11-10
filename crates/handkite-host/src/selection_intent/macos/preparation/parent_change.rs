use super::*;
use crate::release_files::{directory_matches, directory_metadata, open_at};
use std::collections::BTreeMap;
use std::ffi::CStr;

const MAX_ENTRIES: usize = 4096;

#[derive(PartialEq, Eq)]
struct Entry {
    device: i32,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u16,
    links: u16,
    size: i64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn entries(parent: &File) -> Result<BTreeMap<Vec<u8>, Entry>, &'static str> {
    let scan = open_at(parent.as_raw_fd(), c".", true)?;
    let raw = unsafe { libc::fdopendir(scan.as_raw_fd()) };
    if raw.is_null() {
        return Err(UNSAFE);
    }
    std::mem::forget(scan);
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let scan = Directory(raw);
    let mut result = BTreeMap::new();
    loop {
        unsafe { *libc::__error() = 0 };
        let entry = unsafe { libc::readdir(scan.0) };
        if entry.is_null() {
            return if io::Error::last_os_error().raw_os_error() == Some(0) {
                Ok(result)
            } else {
                Err(UNSAFE)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if [b".".as_slice(), b".."].contains(&name.to_bytes()) {
            continue;
        }
        if result.len() >= MAX_ENTRIES {
            return Err("selection releases inventory exceeds limit");
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(UNSAFE);
        }
        let value = unsafe { metadata.assume_init() };
        if result
            .insert(
                name.to_bytes().to_vec(),
                Entry {
                    device: value.st_dev,
                    inode: value.st_ino,
                    uid: value.st_uid,
                    gid: value.st_gid,
                    mode: value.st_mode,
                    links: value.st_nlink,
                    size: value.st_size,
                    modified: (value.st_mtime, value.st_mtime_nsec),
                    changed: (value.st_ctime, value.st_ctime_nsec),
                },
            )
            .is_some()
        {
            return Err(UNSAFE);
        }
    }
}

fn metadata(parent: &File) -> Result<Fingerprint, &'static str> {
    directory_metadata(parent, unsafe { libc::geteuid() }, true)
}

pub(super) struct BeforeChange {
    fingerprint: Fingerprint,
    entries: BTreeMap<Vec<u8>, Entry>,
}

impl BeforeChange {
    pub(super) fn capture(parent: &File, name: &CStr) -> Result<Self, &'static str> {
        let fingerprint = metadata(parent)?;
        let entries = entries(parent)?;
        if entries.contains_key(name.to_bytes()) {
            return Err("selection preparation entry already exists");
        }
        if metadata(parent)? != fingerprint {
            return Err(UNSAFE);
        }
        Ok(Self {
            fingerprint,
            entries,
        })
    }

    /// Accept exactly one added inode. Every previously observed entry stays fixed.
    pub(super) fn finish(
        self,
        parent: &File,
        name: &CStr,
        identity: (i32, u64),
    ) -> Result<ParentChange, &'static str> {
        let after = metadata(parent)?;
        let mut current = entries(parent)?;
        let added = current.remove(name.to_bytes()).ok_or(UNSAFE)?;
        if (added.device, added.inode) != identity
            || current != self.entries
            || !directory_matches(&after, &self.fingerprint, false)
            || metadata(parent)? != after
        {
            return Err(UNSAFE);
        }
        Ok(ParentChange {
            before: self.fingerprint,
            after,
        })
    }
}

/// Constructor is private to the preparation writer's checked single-entry change.
pub(crate) struct ParentChange {
    before: Fingerprint,
    after: Fingerprint,
}

impl ParentChange {
    pub(crate) fn check(
        &self,
        original: &Fingerprint,
        observed: &Fingerprint,
    ) -> Result<(), &'static str> {
        if original != &self.before || observed != &self.after {
            return Err(UNSAFE);
        }
        Ok(())
    }
}
