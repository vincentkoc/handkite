//! Read-only stop inventory for a separately reviewed one-shot broker.
//!
//! Capture a private receipt once, then check that receipt before and after the
//! broker's attempt instead of embedding marker inodes in executable scripts.
//! A changed inventory requires owner review, never automatic recapture/retry.
//! Reconcile the first receipt with already approved stop pins; capture cannot
//! establish whether historical holds were removed before this observation.
//! This is point-in-time evidence, not device/service ownership or permission to
//! move a marker. Existing ownership, private PIN stdin and recovery gates remain.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;

const INVALID: &str = "stop_snapshot_invalid";
const CHANGED: &str = "stop_snapshot_changed";
const MAX_BYTES: usize = 131_072;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl From<&std::fs::Metadata> for Identity {
    fn from(meta: &std::fs::Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Marker {
    name: String,
    identity: Identity,
    size: u64,
    modified: (i64, i64),
    sha256: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    directory: Identity,
    markers: Vec<Marker>,
}

/// No path override, state creation, marker mutation, secret read or device IO.
pub fn capture() -> Result<Vec<u8>, &'static str> {
    #[cfg(target_os = "macos")]
    {
        let home = super::account_home().ok_or(INVALID)?;
        macos::capture_at(&home)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(INVALID)
    }
}

/// Compare the fixed account state with one reviewed private regular-file receipt.
/// A match cannot authorize another attempt, particularly after uncertain digits.
pub fn check(input: &mut File) -> Result<(), &'static str> {
    check_with(input, capture)
}

fn check_with(
    input: &mut File,
    capture: impl FnOnce() -> Result<Vec<u8>, &'static str>,
) -> Result<(), &'static str> {
    let before = input.metadata().map_err(|_| INVALID)?;
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > MAX_BYTES as u64
        || input.stream_position().map_err(|_| INVALID)? != 0
    {
        return Err(INVALID);
    }
    crate::release_verify::check_private_input_acl(input).map_err(|_| INVALID)?;
    let mut bytes = Vec::new();
    (&mut *input)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| INVALID)?;
    let after = input.metadata().map_err(|_| INVALID)?;
    if bytes.len() as u64 != before.len() || !same_file(&before, &after) {
        return Err(INVALID);
    }
    crate::release_verify::check_private_input_acl(input).map_err(|_| INVALID)?;
    validate(&bytes)?;
    compare(&bytes, &capture()?)
}

fn validate(expected: &[u8]) -> Result<(), &'static str> {
    let parsed: Snapshot = serde_json::from_slice(expected).map_err(|_| INVALID)?;
    // Accept only the exact producer format; no duplicate, extra or coerced fields.
    if encode(&parsed)?.as_slice() != expected {
        return Err(INVALID);
    }
    Ok(())
}

fn compare(expected: &[u8], actual: &[u8]) -> Result<(), &'static str> {
    validate(expected)?;
    if expected != actual {
        return Err(CHANGED);
    }
    Ok(())
}

fn encode(snapshot: &Snapshot) -> Result<Vec<u8>, &'static str> {
    let mut bytes = serde_json::to_vec(snapshot).map_err(|_| INVALID)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_BYTES {
        return Err(INVALID);
    }
    Ok(bytes)
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    Identity::from(left) == Identity::from(right)
        && left.nlink() == right.nlink()
        && left.len() == right.len()
        && (
            left.mtime(),
            left.mtime_nsec(),
            left.ctime(),
            left.ctime_nsec(),
        ) == (
            right.mtime(),
            right.mtime_nsec(),
            right.ctime(),
            right.ctime_nsec(),
        )
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::release_files::{
        Fingerprint, check_acl, components, directory_matches, directory_metadata, open_at,
    };
    use std::ffi::{CStr, CString};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    const ACTIVE: &str = "UNLOCK-DISABLED";

    pub(super) fn capture_at(home: &Path) -> Result<Vec<u8>, &'static str> {
        capture_checked(home, || {})
    }

    pub(super) fn capture_checked(
        home: &Path,
        after_read: impl FnOnce(),
    ) -> Result<Vec<u8>, &'static str> {
        let uid = unsafe { libc::geteuid() };
        let mut names = components(home).map_err(|_| INVALID)?;
        let home_depth = names.len();
        names.extend(super::super::STATE_COMPONENTS.map(|name| CString::new(name).unwrap()));
        let mut directories = vec![open_at(libc::AT_FDCWD, c"/", true).map_err(|_| INVALID)?];
        let mut pins: Vec<Fingerprint> = Vec::new();
        for index in 0..=names.len() {
            if index != 0 {
                let parent = directories.last().ok_or(INVALID)?.as_raw_fd();
                directories.push(open_at(parent, &names[index - 1], true).map_err(|_| INVALID)?);
            }
            let current = directories.last().ok_or(INVALID)?;
            if index >= home_depth && current.metadata().map_err(|_| INVALID)?.uid() != uid {
                return Err(INVALID);
            }
            pins.push(directory_metadata(current, uid, index == names.len()).map_err(|_| INVALID)?);
        }
        let directory = directories.last().ok_or(INVALID)?;
        let inventory = inventory(directory)?;
        let mut markers = Vec::new();
        let mut held = Vec::new();
        for name in &inventory {
            let mut file = open_at(
                directory.as_raw_fd(),
                &CString::new(name.as_str()).map_err(|_| INVALID)?,
                false,
            )
            .map_err(|_| INVALID)?;
            let before = file.metadata().map_err(|_| INVALID)?;
            if !before.is_file()
                || before.uid() != uid
                || before.mode() & 0o7777 != 0o600
                || before.nlink() != 1
                || before.len() > 4096
            {
                return Err(INVALID);
            }
            check_acl(file.as_raw_fd(), true).map_err(|_| INVALID)?;
            let mut bytes = Vec::new();
            file.by_ref()
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(|_| INVALID)?;
            let current = open_at(
                directory.as_raw_fd(),
                &CString::new(name.as_str()).unwrap(),
                false,
            )
            .map_err(|_| INVALID)?;
            if bytes.len() as u64 != before.len()
                || !same_file(&before, &file.metadata().map_err(|_| INVALID)?)
                || !same_file(&before, &current.metadata().map_err(|_| INVALID)?)
            {
                return Err(CHANGED);
            }
            check_acl(current.as_raw_fd(), true).map_err(|_| INVALID)?;
            markers.push(Marker {
                name: name.clone(),
                identity: Identity::from(&before),
                size: before.len(),
                modified: (before.mtime(), before.mtime_nsec()),
                sha256: ring::digest::digest(&ring::digest::SHA256, &bytes)
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            });
            held.push((name, file, before));
        }
        if inventory != self::inventory(directory)? {
            return Err(CHANGED);
        }
        after_read();
        for (name, file, before) in &held {
            let current = open_at(
                directory.as_raw_fd(),
                &CString::new(name.as_str()).unwrap(),
                false,
            )
            .map_err(|_| INVALID)?;
            if !same_file(before, &file.metadata().map_err(|_| INVALID)?)
                || !same_file(before, &current.metadata().map_err(|_| INVALID)?)
            {
                return Err(CHANGED);
            }
            check_acl(current.as_raw_fd(), true).map_err(|_| INVALID)?;
        }
        let mut walked = open_at(libc::AT_FDCWD, c"/", true).map_err(|_| INVALID)?;
        for (index, (held, pin)) in directories.iter().zip(&pins).enumerate() {
            if index != 0 {
                walked =
                    open_at(walked.as_raw_fd(), &names[index - 1], true).map_err(|_| INVALID)?;
            }
            for file in [held, &walked] {
                let private = index == names.len();
                let current = directory_metadata(file, uid, private).map_err(|_| INVALID)?;
                if !directory_matches(&current, pin, private) {
                    return Err(CHANGED);
                }
            }
        }
        encode(&Snapshot {
            version: 1,
            directory: Identity::from(&directory.metadata().map_err(|_| INVALID)?),
            markers,
        })
    }

    fn inventory(directory: &File) -> Result<Vec<String>, &'static str> {
        let scan = open_at(directory.as_raw_fd(), c".", true).map_err(|_| INVALID)?;
        let raw = unsafe { libc::fdopendir(scan.as_raw_fd()) };
        if raw.is_null() {
            return Err(INVALID);
        }
        std::mem::forget(scan);
        struct Directory(*mut libc::DIR);
        impl Drop for Directory {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let scan = Directory(raw);
        let mut names = Vec::new();
        for _ in 0..1024 {
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(scan.0) };
            if entry.is_null() {
                if std::io::Error::last_os_error().raw_os_error() != Some(0)
                    || !names.iter().any(|n| n == ACTIVE)
                {
                    return Err(INVALID);
                }
                names.sort();
                return Ok(names);
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if !name.starts_with(ACTIVE.as_bytes()) {
                continue;
            }
            let name = std::str::from_utf8(name).map_err(|_| INVALID)?;
            if name != ACTIVE
                && !(name.starts_with("UNLOCK-DISABLED.")
                    && name.ends_with(".hold")
                    && name.len() <= 240
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
            {
                return Err(INVALID);
            }
            names.push(name.to_owned());
            if names.len() > 256 {
                return Err(INVALID);
            }
        }
        Err(INVALID)
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "stop_snapshot_tests.rs"]
mod tests;
