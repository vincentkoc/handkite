use super::{ManifestDigest, ServiceLayout, parse_target};
use crate::release_files::{
    Fingerprint, components, directory_matches, directory_metadata, open_at,
};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;

const UNSAFE: &str = "unsafe or changed current-release selection";

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    private: bool,
    account_owned: bool,
}

struct Releases {
    components: Vec<CString>,
    directories: Vec<Directory>,
    uid: u32,
}

impl Releases {
    fn open(layout: &ServiceLayout) -> Result<Self, &'static str> {
        let components = components(&layout.releases_directory())?;
        // Fixed suffix below home: Library/Application Support/Handkite/releases.
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
        Ok(Self {
            components,
            directories,
            uid,
        })
    }

    fn directory(&self) -> Result<&File, &'static str> {
        Ok(&self.directories.last().ok_or(UNSAFE)?.file)
    }

    /// Hold the fresh releases descriptor after comparing each root-anchored component.
    fn revalidate(&self) -> Result<File, &'static str> {
        self.walk(false)
    }

    fn walk(&self, parent_changed: bool) -> Result<File, &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            for file in [&held.file, &walked] {
                let current = directory_policy(file, self.uid, held.private, held.account_owned)?;
                if !directory_matches(&current, &held.fingerprint, held.private && !parent_changed)
                {
                    return Err(UNSAFE);
                }
            }
        }
        Ok(walked)
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkFingerprint {
    device: libc::dev_t,
    inode: libc::ino_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    mode: libc::mode_t,
    links: libc::nlink_t,
    size: libc::off_t,
    modified: (libc::time_t, libc::c_long),
    changed: (libc::time_t, libc::c_long),
}

fn link_policy(metadata: &libc::stat, uid: u32) -> Result<LinkFingerprint, &'static str> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFLNK
        || metadata.st_uid != uid
        || metadata.st_nlink != 1
        || metadata.st_size != 64
    {
        return Err(UNSAFE);
    }
    Ok(LinkFingerprint {
        device: metadata.st_dev,
        inode: metadata.st_ino,
        uid: metadata.st_uid,
        gid: metadata.st_gid,
        mode: metadata.st_mode,
        links: metadata.st_nlink,
        size: metadata.st_size,
        modified: (metadata.st_mtime, metadata.st_mtime_nsec),
        changed: (metadata.st_ctime, metadata.st_ctime_nsec),
    })
}

fn link_metadata(
    parent: RawFd,
    name: &CStr,
    uid: u32,
) -> Result<Option<LinkFingerprint>, &'static str> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(UNSAFE)
        };
    }
    link_policy(&unsafe { metadata.assume_init() }, uid).map(Some)
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    fingerprint: LinkFingerprint,
    manifest: ManifestDigest,
}

/// A retained symlink inode and exact target; the destination is never followed.
pub(crate) struct RetainedLink {
    file: File,
    name: CString,
    observation: Observation,
}

impl RetainedLink {
    pub(crate) fn name(&self) -> &CStr {
        &self.name
    }

    pub(crate) fn open(
        parent: &File,
        name: CString,
        manifest: ManifestDigest,
    ) -> Result<Self, &'static str> {
        let uid = unsafe { libc::geteuid() };
        let observation = observe(
            parent,
            &name,
            uid,
            #[cfg(test)]
            &mut None,
        )?
        .ok_or(UNSAFE)?;
        if observation.manifest != manifest {
            return Err(UNSAFE);
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                // Darwin rejects O_NOFOLLOW combined with O_SYMLINK. O_SYMLINK
                // opens the link itself; fstat below refuses every other type.
                libc::O_RDONLY | libc::O_SYMLINK | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(UNSAFE);
        }
        let value = Self {
            file: unsafe { File::from_raw_fd(fd) },
            name,
            observation,
        };
        value.check(parent)?;
        Ok(value)
    }

    pub(crate) fn identity(&self) -> (i32, u64) {
        (
            self.observation.fingerprint.device,
            self.observation.fingerprint.inode,
        )
    }

    pub(crate) fn check(&self, parent: &File) -> Result<(), &'static str> {
        let uid = unsafe { libc::geteuid() };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(self.file.as_raw_fd(), metadata.as_mut_ptr()) } != 0
            || link_policy(&unsafe { metadata.assume_init() }, uid)? != self.observation.fingerprint
            || observe(
                parent,
                &self.name,
                uid,
                #[cfg(test)]
                &mut None,
            )?
            .as_ref()
                != Some(&self.observation)
        {
            return Err(UNSAFE);
        }
        Ok(())
    }

    /// Rebind only a moved inode proven by the transaction's exact parent delta.
    pub(crate) fn rebind_after_rename(
        &mut self,
        parent: &File,
        name: CString,
        change: &crate::selection_intent::macos::preparation::ParentChange,
    ) -> Result<(), &'static str> {
        let changed = change.entry_change(&self.name, &name, self.identity())?;
        let mut expected = self.observation.fingerprint.clone();
        expected.changed = changed;
        let uid = unsafe { libc::geteuid() };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(self.file.as_raw_fd(), metadata.as_mut_ptr()) } != 0
            || link_policy(&unsafe { metadata.assume_init() }, uid)? != expected
        {
            return Err(UNSAFE);
        }
        let observed = observe(
            parent,
            &name,
            uid,
            #[cfg(test)]
            &mut None,
        )?
        .ok_or(UNSAFE)?;
        if observed.fingerprint != expected || observed.manifest != self.observation.manifest {
            return Err(UNSAFE);
        }
        self.name = name;
        self.observation = observed;
        self.check(parent)
    }
}

/// Exact link observation plus retained parent identity; never destination trust.
pub(crate) struct Snapshot {
    held: Releases,
    observation: Option<Observation>,
}

impl Snapshot {
    /// Retain the original current inode, comparing the entire original snapshot.
    pub(crate) fn retain_current(&self) -> Result<Option<RetainedLink>, &'static str> {
        self.reobserve_unchanged()?;
        let Some(original) = &self.observation else {
            return Ok(None);
        };
        let link =
            RetainedLink::open(self.held.directory()?, c"current".into(), original.manifest)?;
        if link.observation != *original {
            return Err(UNSAFE);
        }
        self.reobserve_unchanged()?;
        Ok(Some(link))
    }

    pub(crate) fn selected_link_identity(&self) -> Option<(i32, u64)> {
        self.observation
            .as_ref()
            .map(|value| (value.fingerprint.device, value.fingerprint.inode))
    }

    pub(crate) fn selected_digest(&self) -> Option<ManifestDigest> {
        self.observation.as_ref().map(|value| value.manifest)
    }

    pub(crate) fn capture(layout: &ServiceLayout) -> Result<Self, &'static str> {
        capture(
            layout,
            #[cfg(test)]
            None,
        )
    }

    /// Parent entry changes may be deliberate; the original current link may not change.
    pub(crate) fn reobserve_unchanged(&self) -> Result<(), &'static str> {
        self.reobserve(
            #[cfg(test)]
            None,
        )
    }

    fn reobserve(&self, #[cfg(test)] mut hook: Option<Hook<'_>>) -> Result<(), &'static str> {
        let result = (|| {
            let fresh = self.held.walk(true)?;
            let parent_before = directory_policy(&fresh, self.held.uid, true, true)?;
            let before = observe(
                self.held.directory()?,
                c"current",
                self.held.uid,
                #[cfg(test)]
                &mut hook,
            )?;
            #[cfg(test)]
            if let Some(hook) = hook.as_mut() {
                hook(InspectionPoint::BetweenObservations);
            }
            let after = observe(
                &fresh,
                c"current",
                self.held.uid,
                #[cfg(test)]
                &mut hook,
            )?;
            if before != self.observation || after != self.observation {
                return Err(UNSAFE);
            }
            let final_parent = self.held.walk(true)?;
            if directory_policy(&final_parent, self.held.uid, true, true)? != parent_before {
                return Err(UNSAFE);
            }
            Ok(())
        })();
        result.map_err(|_| UNSAFE)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InspectionPoint {
    AfterMetadata,
    AfterReadlink,
    BetweenObservations,
}

#[cfg(test)]
type Hook<'a> = &'a mut dyn FnMut(InspectionPoint);

fn observe(
    parent: &File,
    name: &CStr,
    uid: u32,
    #[cfg(test)] hook: &mut Option<Hook<'_>>,
) -> Result<Option<Observation>, &'static str> {
    let Some(before) = link_metadata(parent.as_raw_fd(), name, uid)? else {
        return Ok(None);
    };
    #[cfg(test)]
    if let Some(hook) = hook.as_mut() {
        hook(InspectionPoint::AfterMetadata);
    }
    // One extra byte detects a target that grows beyond the exact 64-byte grammar.
    let mut target = [0_u8; 65];
    let size = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    if size != 64 {
        return Err(UNSAFE);
    }
    #[cfg(test)]
    if let Some(hook) = hook.as_mut() {
        hook(InspectionPoint::AfterReadlink);
    }
    if link_metadata(parent.as_raw_fd(), name, uid)?.as_ref() != Some(&before) {
        return Err(UNSAFE);
    }
    Ok(Some(Observation {
        fingerprint: before,
        manifest: parse_target(&target[..64])?,
    }))
}

pub(super) fn inspect(
    layout: &ServiceLayout,
    #[cfg(test)] hook: Option<Hook<'_>>,
) -> Result<Option<ManifestDigest>, &'static str> {
    #[cfg(test)]
    let snapshot = if hook.is_some() {
        capture(layout, hook)?
    } else {
        Snapshot::capture(layout)?
    };
    #[cfg(not(test))]
    let snapshot = Snapshot::capture(layout)?;
    Ok(snapshot.observation.map(|value| value.manifest))
}

fn capture(
    layout: &ServiceLayout,
    #[cfg(test)] mut hook: Option<Hook<'_>>,
) -> Result<Snapshot, &'static str> {
    let result = (|| {
        let held = Releases::open(layout)?;
        let before = observe(
            held.directory()?,
            c"current",
            held.uid,
            #[cfg(test)]
            &mut hook,
        )?;
        #[cfg(test)]
        if let Some(hook) = hook.as_mut() {
            hook(InspectionPoint::BetweenObservations);
        }
        let fresh = held.revalidate()?;
        let after = observe(
            &fresh,
            c"current",
            held.uid,
            #[cfg(test)]
            &mut hook,
        )?;
        if before != after {
            return Err(UNSAFE);
        }
        held.revalidate()?;
        Ok(Snapshot {
            held,
            observation: before,
        })
    })();
    result.map_err(|_| UNSAFE)
}

#[cfg(test)]
mod tests;
