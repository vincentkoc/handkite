use super::{ServiceLayout, UNAVAILABLE};
use crate::release_files::{
    Fingerprint, check_acl, components, directory_matches, directory_metadata, open_at,
};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

struct Directory {
    file: File,
    fingerprint: Fingerprint,
    private: bool,
    account_owned: bool,
}

struct Runtime {
    components: Vec<CString>,
    directories: Vec<Directory>,
    uid: u32,
}

impl Runtime {
    fn open(layout: &ServiceLayout) -> Result<Self, &'static str> {
        let components = components(&layout.runtime_directory())?;
        // Fixed suffix below home: Library/Application Support/Handkite/runtime.
        let home_index = components.len().checked_sub(4).ok_or(UNAVAILABLE)?;
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
                    directories.last().ok_or(UNAVAILABLE)?.file.as_raw_fd(),
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

    fn directory(&self) -> &File {
        &self
            .directories
            .last()
            .expect("root directory is held")
            .file
    }

    fn revalidate(&self, after_creation: bool) -> Result<File, &'static str> {
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
            }
            for file in [&held.file, &walked] {
                let current = directory_policy(file, self.uid, held.private, held.account_owned)?;
                // Creating the lock changes runtime timestamps, never its identity/access.
                if !directory_matches(&current, &held.fingerprint, held.private && !after_creation)
                {
                    return Err(UNAVAILABLE);
                }
            }
        }
        Ok(walked)
    }

    fn accept_creation(&mut self) -> Result<(), &'static str> {
        let fresh = self.revalidate(true)?;
        let held = self.directories.last_mut().ok_or(UNAVAILABLE)?;
        let fingerprint = directory_policy(&held.file, self.uid, true, true)?;
        if directory_policy(&fresh, self.uid, true, true)? != fingerprint {
            return Err(UNAVAILABLE);
        }
        held.fingerprint = fingerprint;
        Ok(())
    }
}

fn directory_policy(
    file: &File,
    uid: u32,
    private: bool,
    account_owned: bool,
) -> Result<Fingerprint, &'static str> {
    let fingerprint = directory_metadata(file, uid, private)?;
    if account_owned && file.metadata().map_err(|_| UNAVAILABLE)?.uid() != uid {
        return Err(UNAVAILABLE);
    }
    Ok(fingerprint)
}

fn file_policy(file: &File, uid: u32) -> Result<Fingerprint, &'static str> {
    let metadata = file.metadata().map_err(|_| UNAVAILABLE)?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(UNAVAILABLE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(metadata.into())
}

fn open_lock(directory: &File, create: bool) -> std::io::Result<File> {
    let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"handkite.lock".as_ptr(),
            flags
                | if create {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) struct Guard {
    runtime: Runtime,
    file: File,
    fingerprint: Fingerprint,
}

impl Guard {
    pub(super) fn revalidate(&self) -> Result<(), &'static str> {
        let directory = self.runtime.revalidate(false)?;
        let fresh = open_lock(&directory, false).map_err(|_| UNAVAILABLE)?;
        for file in [&self.file, &fresh] {
            if file_policy(file, self.runtime.uid)? != self.fingerprint {
                return Err(UNAVAILABLE);
            }
        }
        self.runtime.revalidate(false)?;
        if file_policy(&self.file, self.runtime.uid)? != self.fingerprint {
            return Err(UNAVAILABLE);
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Point {
    AfterAbsent,
    BeforeCreate,
    AfterOpen,
    AfterFlock,
    AfterBaseline,
}

pub(super) fn acquire(layout: &ServiceLayout) -> Result<Guard, &'static str> {
    acquire_inner(
        layout,
        #[cfg(test)]
        &mut |_| {},
    )
    .map_err(|_| UNAVAILABLE)
}

fn acquire_inner(
    layout: &ServiceLayout,
    #[cfg(test)] hook: &mut dyn FnMut(Point),
) -> Result<Guard, &'static str> {
    let mut runtime = Runtime::open(layout)?;
    runtime.revalidate(false)?;
    let (file, created) = match open_lock(runtime.directory(), false) {
        Ok(file) => (file, false),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            #[cfg(test)]
            hook(Point::AfterAbsent);
            runtime.revalidate(false)?;
            #[cfg(test)]
            hook(Point::BeforeCreate);
            (
                open_lock(runtime.directory(), true).map_err(|_| UNAVAILABLE)?,
                true,
            )
        }
        Err(_) => return Err(UNAVAILABLE),
    };
    #[cfg(test)]
    hook(Point::AfterOpen);
    let fingerprint = file_policy(&file, runtime.uid)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(UNAVAILABLE);
    }
    #[cfg(test)]
    hook(Point::AfterFlock);
    if created {
        runtime.accept_creation()?;
    }
    #[cfg(test)]
    hook(Point::AfterBaseline);
    let guard = Guard {
        runtime,
        file,
        fingerprint,
    };
    guard.revalidate()?;
    Ok(guard)
}

#[cfg(test)]
mod tests;
