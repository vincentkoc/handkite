use super::*;
use crate::release_files::{Fingerprint, check_acl, open_at};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

fn policy(file: &File, mode: u32, size: usize) -> Result<Fingerprint, &'static str> {
    let metadata = file.metadata().map_err(|_| UNSAFE)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != mode
        || metadata.nlink() != 1
        || metadata.len() != size as u64
        || size > MAX_INTENT_BYTES
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(metadata.into())
}

pub(super) fn check_writer(
    file: &File,
    parent: &File,
    mode: u32,
    bytes: &[u8],
) -> Result<Fingerprint, &'static str> {
    check_named(file, parent, JOURNAL, mode, bytes)
}

fn check_named(
    file: &File,
    parent: &File,
    name: &CStr,
    mode: u32,
    bytes: &[u8],
) -> Result<Fingerprint, &'static str> {
    let before = policy(file, mode, bytes.len())?;
    let fresh = open_at(parent.as_raw_fd(), name, false)?;
    for mut reader in [file, &fresh] {
        if policy(reader, mode, bytes.len())? != before {
            return Err(UNSAFE);
        }
        reader.seek(SeekFrom::Start(0)).map_err(|_| UNSAFE)?;
        let mut actual = Vec::new();
        reader
            .take(MAX_INTENT_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|_| UNSAFE)?;
        if actual != bytes || policy(reader, mode, bytes.len())? != before {
            return Err(UNSAFE);
        }
    }
    Ok(before)
}

pub(super) struct Journal {
    name: CString,
    file: File,
    fingerprint: Fingerprint,
    bytes: Vec<u8>,
}

impl Journal {
    pub(super) fn open_existing(parent: &File) -> Result<(Self, Intent), &'static str> {
        let mut file = open_at(parent.as_raw_fd(), JOURNAL, false)?;
        let size =
            usize::try_from(file.metadata().map_err(|_| UNSAFE)?.len()).map_err(|_| UNSAFE)?;
        let fingerprint = policy(&file, 0o400, size)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_INTENT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| UNSAFE)?;
        let intent = Intent::parse(&bytes)?;
        let value = Self {
            name: JOURNAL.into(),
            file,
            fingerprint,
            bytes,
        };
        value.check(parent)?;
        Ok((value, intent))
    }

    pub(super) fn retain(
        writer: File,
        parent: &File,
        bytes: Vec<u8>,
    ) -> Result<Self, &'static str> {
        let fingerprint = check_writer(&writer, parent, 0o400, &bytes)?;
        drop(writer);
        let file = open_at(parent.as_raw_fd(), JOURNAL, false)?;
        let value = Self {
            name: JOURNAL.into(),
            file,
            fingerprint,
            bytes,
        };
        value.check(parent)?;
        Ok(value)
    }

    pub(super) fn check(&self, parent: &File) -> Result<(), &'static str> {
        if check_named(&self.file, parent, &self.name, 0o400, &self.bytes)? != self.fingerprint {
            return Err(UNSAFE);
        }
        Ok(())
    }

    pub(super) fn completion_name(&self) -> Result<CString, &'static str> {
        let intent = Intent::parse(&self.bytes)?;
        let transaction: String = intent
            .transaction
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        CString::new(format!(".selection-completed-{transaction}.json")).map_err(|_| UNSAFE)
    }

    pub(super) fn identity(&self) -> Result<(i32, u64), &'static str> {
        let metadata = self.file.metadata().map_err(|_| UNSAFE)?;
        if Fingerprint::from(metadata.clone()) != self.fingerprint {
            return Err(UNSAFE);
        }
        Ok((metadata.dev() as i32, metadata.ino()))
    }

    /// Only an exact checked move may advance this held journal's name and ctime.
    pub(super) fn rebind_after_rename(
        &mut self,
        parent: &File,
        name: CString,
        change: &preparation::ParentChange,
    ) -> Result<(), &'static str> {
        let metadata = self.file.metadata().map_err(|_| UNSAFE)?;
        let changed =
            change.entry_change(&self.name, &name, (metadata.dev() as i32, metadata.ino()))?;
        if changed != (metadata.ctime(), metadata.ctime_nsec()) {
            return Err(UNSAFE);
        }
        let fingerprint = Fingerprint::from(metadata);
        if !fingerprint.matches_after_rename(&self.fingerprint)
            || check_named(&self.file, parent, &name, 0o400, &self.bytes)? != fingerprint
        {
            return Err(UNSAFE);
        }
        self.name = name;
        self.fingerprint = fingerprint;
        self.check(parent)
    }
}
