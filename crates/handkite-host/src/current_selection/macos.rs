use super::{ManifestDigest, ServiceLayout, parse_target};
use crate::release_files::{
    Fingerprint, components, directory_matches, directory_metadata, open_at,
};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
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

#[derive(Debug, PartialEq, Eq)]
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

fn link_metadata(parent: RawFd, uid: u32) -> Result<Option<LinkFingerprint>, &'static str> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent,
            c"current".as_ptr(),
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

/// Exact link observation plus retained parent identity; never destination trust.
pub(crate) struct Snapshot {
    held: Releases,
    observation: Option<Observation>,
}

impl Snapshot {
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
    uid: u32,
    #[cfg(test)] hook: &mut Option<Hook<'_>>,
) -> Result<Option<Observation>, &'static str> {
    let Some(before) = link_metadata(parent.as_raw_fd(), uid)? else {
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
            c"current".as_ptr(),
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
    if link_metadata(parent.as_raw_fd(), uid)?.as_ref() != Some(&before) {
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
