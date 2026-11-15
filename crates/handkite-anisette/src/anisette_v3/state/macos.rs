use super::*;
use crate::fs_anchor::{
    Fingerprint, check_acl, components, directory_matches, directory_metadata, open_at,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use zeroize::Zeroizing;

const INVALID: &str = "Anisette state is unsafe, incomplete or changed";
const RESERVED: &str = "Anisette provisioning is already reserved";
const ORIGIN: &str = "https://anisette.example.invalid";
const BACKEND: &str = "macos-adid";
const VERSION: u32 = 1;
const ATTEMPT_LIMIT: usize = 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    origin: String,
    backend: String,
    version: u32,
    identifier: [u8; 16],
}

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    private: bool,
}

struct Anchor {
    names: Vec<CString>,
    directories: Vec<Directory>,
    uid: u32,
}

impl Anchor {
    fn open(layout: &StateLayout) -> Result<Self, &'static str> {
        let names = components(&layout.state_directory()).map_err(|_| INVALID)?;
        let uid = unsafe { libc::geteuid() };
        let root = open_at(libc::AT_FDCWD, c"/", true).map_err(|_| INVALID)?;
        let mut directories = vec![Directory {
            fingerprint: directory_metadata(&root, uid, false).map_err(|_| INVALID)?,
            file: root,
            private: false,
        }];
        for (index, name) in names.iter().enumerate() {
            let parent = &directories.last().ok_or(INVALID)?.file;
            let file = open_at(parent.as_raw_fd(), name, true).map_err(|_| INVALID)?;
            // Handkite and state must be private account-owned directories.
            let private = index + 2 >= names.len();
            directories.push(Directory {
                fingerprint: directory_metadata(&file, uid, private).map_err(|_| INVALID)?,
                file,
                private,
            });
        }
        let value = Self {
            names,
            directories,
            uid,
        };
        value.revalidate()?;
        Ok(value)
    }

    fn parent(&self) -> &File {
        &self.directories.last().expect("state anchor").file
    }

    fn revalidate(&self) -> Result<(), &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true).map_err(|_| INVALID)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.names[index - 1], true)
                    .map_err(|_| INVALID)?;
            }
            for file in [&held.file, &walked] {
                let current =
                    directory_metadata(file, self.uid, held.private).map_err(|_| INVALID)?;
                // Entries in shared ancestors can change; identity/access cannot.
                if !directory_matches(&current, &held.fingerprint, false) {
                    return Err(INVALID);
                }
            }
        }
        Ok(())
    }
}

struct ProtectedFile {
    file: File,
    fingerprint: Fingerprint,
    bytes: Zeroizing<Vec<u8>>,
}

impl ProtectedFile {
    fn open(parent: &File, name: &CStr, uid: u32, limit: usize) -> Result<Self, &'static str> {
        let mut file = open_at(parent.as_raw_fd(), name, false).map_err(|_| INVALID)?;
        let fingerprint = file_metadata(&file, uid)?;
        if fingerprint.size == 0 || fingerprint.size > limit as u64 {
            return Err(INVALID);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        (&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| INVALID)?;
        if bytes.len() as u64 != fingerprint.size || file_metadata(&file, uid)? != fingerprint {
            return Err(INVALID);
        }
        Ok(Self {
            file,
            fingerprint,
            bytes,
        })
    }

    fn revalidate(&self, parent: &File, name: &CStr, uid: u32) -> Result<(), &'static str> {
        let current = Self::open(parent, name, uid, self.bytes.len())?;
        if file_metadata(&self.file, uid)? != self.fingerprint
            || current.fingerprint != self.fingerprint
            || current.bytes != self.bytes
        {
            return Err(INVALID);
        }
        Ok(())
    }
}

pub(super) struct Pending {
    anchor: Anchor,
    directory: Directory,
    attempt: ProtectedFile,
    identifier: Zeroizing<[u8; 16]>,
}

pub(super) fn reserve(layout: &StateLayout) -> Result<Pending, &'static str> {
    let anchor = Anchor::open(layout)?;
    let mut identifier = Zeroizing::new([0; 16]);
    SystemRandom::new()
        .fill(&mut *identifier)
        .map_err(|_| "Anisette identifier generation failed")?;
    let bytes = Zeroizing::new(
        serde_json::to_vec(&Attempt {
            origin: ORIGIN.into(),
            backend: BACKEND.into(),
            version: VERSION,
            identifier: *identifier,
        })
        .map_err(|_| INVALID)?,
    );
    anchor.revalidate()?;
    if unsafe { libc::mkdirat(anchor.parent().as_raw_fd(), c"anisette-v3".as_ptr(), 0o700) } != 0 {
        return Err(
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                RESERVED
            } else {
                INVALID
            },
        );
    }
    // Nothing below removes residue, including a failure immediately after mkdir.
    let file = open_at(anchor.parent().as_raw_fd(), c"anisette-v3", true).map_err(|_| INVALID)?;
    directory_metadata(&file, anchor.uid, true).map_err(|_| INVALID)?;
    inventory(&file, &[])?;
    let attempt = create_file(&file, c"attempt.json", &bytes, anchor.uid)?;
    attempt.file.sync_all().map_err(|_| INVALID)?;
    file.sync_all().map_err(|_| INVALID)?;
    anchor.parent().sync_all().map_err(|_| INVALID)?;
    let directory = Directory {
        fingerprint: directory_metadata(&file, anchor.uid, true).map_err(|_| INVALID)?,
        file,
        private: true,
    };
    let value = Pending {
        anchor,
        directory,
        attempt,
        identifier,
    };
    value.revalidate(&[b"attempt.json"])?;
    Ok(value)
}

impl Pending {
    pub(super) fn identifier(&self) -> &[u8; 16] {
        &self.identifier
    }

    fn revalidate(&self, names: &[&[u8]]) -> Result<(), &'static str> {
        self.anchor.revalidate()?;
        let current =
            open_at(self.anchor.parent().as_raw_fd(), c"anisette-v3", true).map_err(|_| INVALID)?;
        for file in [&self.directory.file, &current] {
            let fingerprint =
                directory_metadata(file, self.anchor.uid, true).map_err(|_| INVALID)?;
            if !directory_matches(&fingerprint, &self.directory.fingerprint, false) {
                return Err(INVALID);
            }
            inventory(file, names)?;
        }
        self.attempt
            .revalidate(&current, c"attempt.json", self.anchor.uid)
    }

    pub(super) fn revalidate_pending(&self) -> Result<(), &'static str> {
        self.revalidate(&[b"attempt.json"])
    }

    pub(super) fn persist(self, state: &MacosProvisionedState) -> Result<(), &'static str> {
        if state.identifier != *self.identifier {
            return Err(INVALID);
        }
        self.revalidate(&[b"attempt.json"])?;
        let stage = create_file(
            &self.directory.file,
            c"ready.stage",
            state.as_bytes(),
            self.anchor.uid,
        )?;
        stage.file.sync_all().map_err(|_| INVALID)?;
        self.revalidate(&[b"attempt.json", b"ready.stage"])?;
        stage.revalidate(&self.directory.file, c"ready.stage", self.anchor.uid)?;
        if unsafe {
            libc::renameatx_np(
                self.directory.file.as_raw_fd(),
                c"ready.stage".as_ptr(),
                self.directory.file.as_raw_fd(),
                c"ready.json".as_ptr(),
                libc::RENAME_EXCL,
            )
        } != 0
        {
            return Err(INVALID);
        }
        self.directory.file.sync_all().map_err(|_| INVALID)?;
        self.anchor.parent().sync_all().map_err(|_| INVALID)?;
        self.revalidate(&[b"attempt.json", b"ready.json"])?;
        let ready = ProtectedFile::open(
            &self.directory.file,
            c"ready.json",
            self.anchor.uid,
            super::super::MAX_ADI_STATE_BYTES,
        )?;
        if !ready.fingerprint.matches_after_rename(&stage.fingerprint)
            || !file_metadata(&stage.file, self.anchor.uid)?
                .matches_after_rename(&stage.fingerprint)
            || ready.bytes.as_slice() != state.as_bytes()
        {
            return Err(INVALID);
        }
        Ok(())
    }
}

pub(super) fn load(layout: &StateLayout) -> Result<MacosProvisionedState, &'static str> {
    let anchor = Anchor::open(layout)?;
    let file = open_at(anchor.parent().as_raw_fd(), c"anisette-v3", true).map_err(|_| INVALID)?;
    let directory = Directory {
        fingerprint: directory_metadata(&file, anchor.uid, true).map_err(|_| INVALID)?,
        file,
        private: true,
    };
    inventory(&directory.file, &[b"attempt.json", b"ready.json"])?;
    let attempt = ProtectedFile::open(&directory.file, c"attempt.json", anchor.uid, ATTEMPT_LIMIT)?;
    let parsed: Attempt = serde_json::from_slice(&attempt.bytes).map_err(|_| INVALID)?;
    if parsed.origin != ORIGIN || parsed.backend != BACKEND || parsed.version != VERSION {
        return Err(INVALID);
    }
    let ready = ProtectedFile::open(
        &directory.file,
        c"ready.json",
        anchor.uid,
        super::super::MAX_ADI_STATE_BYTES,
    )?;
    let state = MacosProvisionedState::parse(ready.bytes.to_vec(), parsed.identifier)
        .map_err(|_| INVALID)?;
    let retained = Pending {
        anchor,
        directory,
        attempt,
        identifier: Zeroizing::new(parsed.identifier),
    };
    retained.revalidate(&[b"attempt.json", b"ready.json"])?;
    ready.revalidate(&retained.directory.file, c"ready.json", retained.anchor.uid)?;
    Ok(state)
}

fn file_metadata(file: &File, uid: u32) -> Result<Fingerprint, &'static str> {
    let meta = file.metadata().map_err(|_| INVALID)?;
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o7777 != 0o400 {
        return Err(INVALID);
    }
    check_acl(file.as_raw_fd(), true).map_err(|_| INVALID)?;
    Ok(meta.into())
}

fn create_file(
    parent: &File,
    name: &CStr,
    bytes: &[u8],
    uid: u32,
) -> Result<ProtectedFile, &'static str> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(INVALID);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(|_| INVALID)?;
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o7777 != 0o600 {
        return Err(INVALID);
    }
    check_acl(file.as_raw_fd(), true).map_err(|_| INVALID)?;
    file.write_all(bytes).map_err(|_| INVALID)?;
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o400) } != 0 {
        return Err(INVALID);
    }
    let fingerprint = file_metadata(&file, uid)?;
    let opened = ProtectedFile::open(parent, name, uid, bytes.len())?;
    if opened.fingerprint != fingerprint || opened.bytes.as_slice() != bytes {
        return Err(INVALID);
    }
    Ok(opened)
}

fn inventory(directory: &File, expected: &[&[u8]]) -> Result<(), &'static str> {
    let scan = open_at(directory.as_raw_fd(), c".", true).map_err(|_| INVALID)?;
    let raw = unsafe { libc::fdopendir(scan.as_raw_fd()) };
    if raw.is_null() {
        return Err(INVALID);
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
                && seen.iter().all(|x| *x)
            {
                Ok(())
            } else {
                Err(INVALID)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let index = expected
            .iter()
            .position(|item| *item == name)
            .ok_or(INVALID)?;
        if seen[index] {
            return Err(INVALID);
        }
        seen[index] = true;
    }
}
