use super::*;
use crate::release_files::{check_acl, open_at};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;

pub(crate) struct Receipt {
    pub(super) file: File,
    fingerprint: Fingerprint,
    name: CString,
}

fn policy(file: &File, mode: u32, size: usize) -> Result<Fingerprint, &'static str> {
    let metadata = file.metadata().map_err(|_| UNSAFE)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != mode
        || metadata.nlink() != 1
        || metadata.len() != size as u64
        || size > crate::release_manifest::MAX_MANIFEST_BYTES
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(metadata.into())
}

fn check_file(file: &File, mode: u32, bytes: &[u8]) -> Result<Fingerprint, &'static str> {
    let before = policy(file, mode, bytes.len())?;
    let mut reader = file;
    reader.seek(SeekFrom::Start(0)).map_err(|_| UNSAFE)?;
    let mut limited = reader.take(bytes.len() as u64 + 1);
    let mut buffer = [0; 4096];
    let mut offset = 0;
    loop {
        let count = limited.read(&mut buffer).map_err(|_| UNSAFE)?;
        if count == 0 {
            break;
        }
        let end = offset + count;
        if end > bytes.len() || bytes[offset..end] != buffer[..count] {
            return Err(UNSAFE);
        }
        offset = end;
    }
    if offset != bytes.len() || policy(file, mode, bytes.len())? != before {
        return Err(UNSAFE);
    }
    Ok(before)
}

pub(super) fn check_named(
    file: &File,
    parent: &File,
    name: &CStr,
    mode: u32,
    bytes: &[u8],
) -> Result<Fingerprint, &'static str> {
    let before = check_file(file, mode, bytes)?;
    let fresh = open_at(parent.as_raw_fd(), name, false)?;
    if check_file(&fresh, mode, bytes)? != before || policy(file, mode, bytes.len())? != before {
        return Err(UNSAFE);
    }
    Ok(before)
}

impl Receipt {
    pub(super) fn open_existing(
        parent: &File,
        name: CString,
        bytes: &[u8],
    ) -> Result<Self, &'static str> {
        let file = open_at(parent.as_raw_fd(), &name, false)?;
        let fingerprint = check_named(&file, parent, &name, 0o400, bytes)?;
        Ok(Self {
            file,
            fingerprint,
            name,
        })
    }

    pub(super) fn reopen(
        writer: File,
        parent: &File,
        name: CString,
        bytes: &[u8],
    ) -> Result<Self, &'static str> {
        let fingerprint = check_named(&writer, parent, &name, 0o400, bytes)?;
        drop(writer);
        let file = open_at(parent.as_raw_fd(), &name, false)?;
        if check_named(&file, parent, &name, 0o400, bytes)? != fingerprint {
            return Err(UNSAFE);
        }
        Ok(Self {
            file,
            fingerprint,
            name,
        })
    }

    pub(super) fn check(&self, parent: &File, bytes: &[u8]) -> Result<(), &'static str> {
        if check_named(&self.file, parent, &self.name, 0o400, bytes)? != self.fingerprint {
            return Err(UNSAFE);
        }
        Ok(())
    }

    pub(super) fn rebind(
        &mut self,
        parent: &File,
        name: CString,
        bytes: &[u8],
    ) -> Result<(), &'static str> {
        let fingerprint = check_named(&self.file, parent, &name, 0o400, bytes)?;
        if !fingerprint.matches_after_rename(&self.fingerprint) {
            return Err(UNSAFE);
        }
        self.name = name;
        self.fingerprint = fingerprint;
        self.check(parent, bytes)
    }
}
