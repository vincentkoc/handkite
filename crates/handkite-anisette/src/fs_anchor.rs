//! Private copy of the root-anchored, nofollow filesystem checks used by the
//! Handkite host release files. Kept local so this optional crate needs no
//! dependency on, or public API from, `handkite-host`. Keep both in step.

use std::ffi::{CStr, CString, c_void};
use std::fs::{File, Metadata};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const UNSAFE: &str = "unsafe or changed Anisette files";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    links: u64,
    pub(crate) size: u64,
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
    pub(crate) fn matches_after_rename(&self, original: &Self) -> bool {
        let mut compared = self.clone();
        compared.changed = original.changed;
        compared == *original
    }
}

pub(crate) fn directory_matches(current: &Fingerprint, held: &Fingerprint, private: bool) -> bool {
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

pub(crate) fn components(path: &Path) -> Result<Vec<CString>, &'static str> {
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

pub(crate) fn open_at(parent: RawFd, name: &CStr, directory: bool) -> Result<File, &'static str> {
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

pub(crate) fn directory_metadata(
    file: &File,
    uid: u32,
    private: bool,
) -> Result<Fingerprint, &'static str> {
    use std::os::fd::AsRawFd;
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

pub(crate) fn check_acl(fd: RawFd, require_empty: bool) -> Result<(), &'static str> {
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
