//! Private read-only anchors for a staged release. No installation capability.

use crate::release_manifest::ArtifactName;
use std::ffi::{CStr, CString, c_void};
use std::fs::{File, Metadata};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

mod publication;

pub(super) const NAMES: [ArtifactName; 3] =
    [ArtifactName::Daemon, ArtifactName::Cli, ArtifactName::Mcp];
const UNSAFE: &str = "unsafe or changed release files";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fingerprint {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    links: u64,
    pub(super) size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<Metadata> for Fingerprint {
    fn from(meta: Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
            links: meta.nlink(),
            size: meta.size(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

impl Fingerprint {
    /// A rename may change ctime, but not identity, access, contents, or mtime.
    pub(super) fn matches_after_rename(&self, original: &Self) -> bool {
        let mut compared = self.clone();
        compared.changed = original.changed;
        compared == *original
    }
}

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    private: bool,
}

pub(super) struct ArtifactFile {
    pub(super) file: File,
    pub(super) fingerprint: Fingerprint,
}

pub(super) struct AnchoredRelease {
    path: PathBuf,
    components: Vec<CString>,
    directories: Vec<Directory>,
    pub(super) artifacts: [ArtifactFile; 3],
    uid: u32,
}

impl AnchoredRelease {
    pub(super) fn open(path: &Path) -> Result<Self, &'static str> {
        let components = components(path)?;
        let uid = unsafe { libc::geteuid() };
        let root = open_at(libc::AT_FDCWD, c"/", true)?;
        let mut directories = vec![directory(root, uid, false)?];
        for (index, component) in components.iter().enumerate() {
            let parent = &directories.last().ok_or(UNSAFE)?.file;
            let file = open_at(parent.as_raw_fd(), component, true)?;
            // The stage and its immediate parent are private account-owned dirs.
            directories.push(directory(file, uid, index + 2 >= components.len())?);
        }
        let stage = &directories.last().ok_or(UNSAFE)?.file;
        inventory(stage)?;
        let mut opened = Vec::with_capacity(3);
        for name in NAMES {
            let file = open_at(
                stage.as_raw_fd(),
                &CString::new(name.filename()).unwrap(),
                false,
            )?;
            let fingerprint = artifact_metadata(&file, uid)?;
            opened.push(ArtifactFile { file, fingerprint });
        }
        let value = Self {
            path: path.into(),
            components,
            directories,
            artifacts: opened.try_into().map_err(|_| UNSAFE)?,
            uid,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub(super) fn path(&self, name: ArtifactName) -> PathBuf {
        self.path.join(name.filename())
    }

    /// Descriptor identities in stage, parent order; no path resolution.
    pub(super) fn private_directory_identities(&self) -> Result<[(u64, u64); 2], &'static str> {
        let mut identities = [(0, 0); 2];
        for (slot, directory) in identities.iter_mut().zip(self.directories.iter().rev()) {
            let metadata = directory.file.metadata().map_err(|_| UNSAFE)?;
            *slot = (metadata.dev(), metadata.ino());
        }
        Ok(identities)
    }

    /// Compare held descriptors with a fresh nofollow walk from the root.
    pub(super) fn revalidate(&self) -> Result<(), &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            for file in [&held.file, &walked] {
                let current = directory_metadata(file, self.uid, held.private)?;
                if !directory_matches(&current, &held.fingerprint, held.private) {
                    return Err(UNSAFE);
                }
            }
        }
        self.revalidate_artifacts(&walked)
    }

    fn revalidate_artifacts(&self, directory: &File) -> Result<(), &'static str> {
        inventory(directory)?;
        for (name, held) in NAMES.into_iter().zip(&self.artifacts) {
            let current = open_at(
                directory.as_raw_fd(),
                &CString::new(name.filename()).unwrap(),
                false,
            )?;
            for file in [&held.file, &current] {
                if artifact_metadata(file, self.uid)? != held.fingerprint {
                    return Err(UNSAFE);
                }
            }
        }
        Ok(())
    }
}

pub(super) fn directory_matches(current: &Fingerprint, held: &Fingerprint, private: bool) -> bool {
    if private {
        return current == held;
    }
    // Unrelated entries in higher ancestors may change while fixtures or other
    // account work run. Their path identity and access policy must stay fixed.
    (
        current.device,
        current.inode,
        current.uid,
        current.gid,
        current.mode,
    ) == (held.device, held.inode, held.uid, held.gid, held.mode)
}

pub(super) fn components(path: &Path) -> Result<Vec<CString>, &'static str> {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") || bytes.len() > 4096 {
        return Err(UNSAFE);
    }
    let parts = bytes[1..]
        .split(|byte| *byte == b'/')
        .map(|part| {
            if part.is_empty() || part == b"." || part == b".." {
                return Err(UNSAFE);
            }
            CString::new(part).map_err(|_| UNSAFE)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !(2..=64).contains(&parts.len()) {
        return Err(UNSAFE);
    }
    Ok(parts)
}

pub(super) fn open_at(parent: RawFd, name: &CStr, directory: bool) -> Result<File, &'static str> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        return Err(UNSAFE);
    }
    // openat returned a new descriptor; this File now owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn directory(file: File, uid: u32, private: bool) -> Result<Directory, &'static str> {
    let fingerprint = directory_metadata(&file, uid, private)?;
    Ok(Directory {
        file,
        fingerprint,
        private,
    })
}

pub(super) fn directory_metadata(
    file: &File,
    uid: u32,
    private: bool,
) -> Result<Fingerprint, &'static str> {
    let meta = file.metadata().map_err(|_| UNSAFE)?;
    if !meta.is_dir()
        || (meta.uid() != uid && (private || meta.uid() != 0))
        || meta.nlink() == 0
        || meta.mode() & 0o7022 != 0
        || (private && meta.mode() & 0o7777 != 0o700)
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), private)?;
    Ok(meta.into())
}

fn artifact_metadata(file: &File, uid: u32) -> Result<Fingerprint, &'static str> {
    let meta = file.metadata().map_err(|_| UNSAFE)?;
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o7777 != 0o500 {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(meta.into())
}

fn inventory(directory: &File) -> Result<(), &'static str> {
    // A separate open gives each scan its own directory offset. fdopendir owns it.
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
    let mut seen = [false; 3];
    loop {
        unsafe { *libc::__error() = 0 };
        let entry = unsafe { libc::readdir(scan.0) };
        if entry.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(0) && seen == [true; 3]
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
        let index = inventory_index(name).ok_or(UNSAFE)?;
        if seen[index] {
            return Err(UNSAFE);
        }
        seen[index] = true;
    }
}

fn inventory_index(name: &[u8]) -> Option<usize> {
    NAMES
        .iter()
        .position(|item| item.filename().as_bytes() == name)
}

// Signatures/constants come from Apple's sys/acl.h. Darwin returns 0 for an
// entry and -1/EINVAL at exhaustion (unlike POSIX implementations returning 1).
unsafe extern "C" {
    fn filesec_init() -> *mut c_void;
    fn filesec_free(security: *mut c_void);
    fn filesec_query_property(
        security: *mut c_void,
        property: libc::c_int,
        present: *mut libc::c_int,
    ) -> libc::c_int;
    fn filesec_get_property(
        security: *mut c_void,
        property: libc::c_int,
        value: *mut c_void,
    ) -> libc::c_int;
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(fd: libc::c_int, stat: *mut libc::stat, security: *mut c_void) -> libc::c_int;
    fn acl_valid(acl: *mut c_void) -> libc::c_int;
    fn acl_get_entry(
        acl: *mut c_void,
        entry_id: libc::c_int,
        entry: *mut *mut c_void,
    ) -> libc::c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
    fn acl_free(acl: *mut c_void) -> libc::c_int;
}

pub(super) fn check_acl(fd: RawFd, require_empty: bool) -> Result<(), &'static str> {
    // Query presence explicitly: acl_get_fd_np uses NULL/ENOENT both for absent
    // ACL metadata and failures, so it cannot by itself prove safe absence.
    let security = unsafe { filesec_init() };
    if security.is_null() {
        return Err(UNSAFE);
    }
    let loaded = (|| {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { fstatx_np(fd, stat.as_mut_ptr(), security) } != 0 {
            return Err(UNSAFE);
        }
        let mut present = 0;
        if unsafe { filesec_query_property(security, 5, &mut present) } != 0 {
            return Err(UNSAFE);
        }
        if present == 0 {
            return Ok(None);
        }
        let mut acl: *mut c_void = std::ptr::null_mut();
        if unsafe { filesec_get_property(security, 5, (&mut acl as *mut *mut c_void).cast()) } != 0
            || acl.is_null()
            || acl as usize == 1
        {
            return Err(UNSAFE);
        }
        Ok(Some(acl))
    })();
    unsafe { filesec_free(security) };
    let Some(acl) = loaded? else {
        return Ok(());
    };
    let result = (|| {
        if unsafe { acl_valid(acl) } != 0 {
            return Err(UNSAFE);
        }
        for index in 0..=128 {
            let mut entry = std::ptr::null_mut();
            let result = unsafe { acl_get_entry(acl, if index == 0 { 0 } else { -1 }, &mut entry) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
            {
                return Ok(());
            }
            if result != 0 || entry.is_null() || require_empty {
                return Err(UNSAFE);
            }
            let mut tag = 0;
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 || tag != 2 {
                return Err(UNSAFE); // Only ACL_EXTENDED_DENY is safe on ancestors.
            }
        }
        Err(UNSAFE)
    })();
    if unsafe { acl_free(acl) } != 0 {
        return Err(UNSAFE);
    }
    result
}

#[cfg(test)]
pub(super) mod tests;
