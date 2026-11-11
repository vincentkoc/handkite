use super::{
    InstallerLock, PublishedRelease, RetainedConfig, StageFailure, StagePhase, StagedDescriptor,
};
use crate::release_files::open_at;
use ring::rand::{SecureRandom, SystemRandom};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

mod files;
use files::{Anchors, Descriptor, FINAL, UNSAFE, bytes_match, file_policy};

#[cfg(test)]
pub(crate) type Check<'a> = &'a mut dyn FnMut(
    crate::release_manifest::ArtifactName,
    &std::path::Path,
) -> Result<(), &'static str>;
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    BeforeCreate,
    Created,
    Written,
    Readonly,
    Synced,
    BeforeRename,
    BeforeRenameCall,
    Renamed,
    BeforeParentSync,
    Complete,
}
#[cfg(test)]
pub(crate) struct Hooks<'a> {
    pub(crate) check: Option<Check<'a>>,
    pub(crate) fault: Option<&'a mut dyn FnMut(Point) -> Result<(), &'static str>>,
    pub(crate) nonce: Option<[u8; 16]>,
    pub(crate) rename_error: Option<(i32, bool)>,
    pub(crate) renames: usize,
}
#[cfg(test)]
impl Hooks<'_> {
    pub(crate) fn production() -> Self {
        Self {
            check: None,
            fault: None,
            nonce: None,
            rename_error: None,
            renames: 0,
        }
    }
    fn at(&mut self, point: Point) -> Result<(), &'static str> {
        if let Some(fault) = self.fault.as_mut() {
            fault(point)?;
        }
        Ok(())
    }
}

struct Inputs<'held, 'lock> {
    owner: &'lock InstallerLock,
    config: &'held mut RetainedConfig<'lock>,
    release: &'held mut PublishedRelease<'lock>,
}

impl Inputs<'_, '_> {
    fn custody(&mut self) -> Result<(), &'static str> {
        self.config.revalidate_for_owner(self.owner)?;
        if self.config.wda_runner_bundle_id().is_none() {
            return Err("configured runner ID required");
        }
        self.release.revalidate_custody()?;
        self.owner.revalidate()
    }
    fn verify(&mut self, #[cfg(test)] check: Option<Check<'_>>) -> Result<(), &'static str> {
        self.custody()?;
        self.release.revalidate_for_owner(
            self.owner,
            #[cfg(test)]
            check,
        )?;
        self.custody()
    }
}

pub(super) struct Staged<'held, 'lock> {
    inputs: Inputs<'held, 'lock>,
    anchors: Anchors,
    descriptor: Descriptor,
}

impl Staged<'_, '_> {
    pub(super) fn revalidate(
        &mut self,
        #[cfg(test)] check: Option<Check<'_>>,
    ) -> Result<(), &'static str> {
        self.anchors.check(&[FINAL])?;
        self.descriptor.check(&self.anchors, FINAL)?;
        self.inputs.verify(
            #[cfg(test)]
            check,
        )?;
        self.descriptor.check(&self.anchors, FINAL)?;
        self.anchors.check(&[FINAL])
    }
}

fn name(#[cfg(test)] hooks: &Hooks<'_>) -> Result<CString, &'static str> {
    let mut nonce = [0; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "descriptor name unavailable")?;
    #[cfg(test)]
    if let Some(fixed) = hooks.nonce {
        nonce = fixed;
    }
    CString::new(format!(
        ".descriptor-{}",
        nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| UNSAFE)
}

fn rename(parent: &File, temporary: &CStr, #[cfg(test)] hooks: &mut Hooks<'_>) -> io::Result<()> {
    #[cfg(test)]
    {
        hooks.renames += 1;
        if let Some((code, false)) = hooks.rename_error {
            return Err(io::Error::from_raw_os_error(code));
        }
    }
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            parent.as_raw_fd(),
            FINAL.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    #[cfg(test)]
    if let Some((code, true)) = hooks.rename_error {
        return Err(io::Error::from_raw_os_error(code));
    }
    Ok(())
}

pub(crate) fn reopen_staged<'held, 'lock>(
    owner: &'lock InstallerLock,
    config: &'held mut RetainedConfig<'lock>,
    release: &'held mut PublishedRelease<'lock>,
    #[cfg(test)] mut check: Option<Check<'_>>,
) -> Result<StagedDescriptor<'held, 'lock>, &'static str> {
    let mut inputs = Inputs {
        owner,
        config,
        release,
    };
    inputs.verify(
        #[cfg(test)]
        check.as_mut().map(|check| &mut **check as _),
    )?;
    let bytes = crate::launchd_descriptor::render(owner.layout())?
        .into_bytes()
        .into_boxed_slice();
    let anchors = Anchors::open(owner.layout(), &[FINAL])?;
    let descriptor = Descriptor::open(&anchors, bytes)?;
    let mut value = Staged {
        inputs,
        anchors,
        descriptor,
    };
    value.revalidate(
        #[cfg(test)]
        check,
    )?;
    Ok(StagedDescriptor { inner: value })
}

pub(crate) fn stage<'held, 'lock>(
    owner: &'lock InstallerLock,
    config: &'held mut RetainedConfig<'lock>,
    release: &'held mut PublishedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<StagedDescriptor<'held, 'lock>, StageFailure> {
    let mut phase = StagePhase::BeforeRename;
    let mut residue = false;
    let inputs = Inputs {
        owner,
        config,
        release,
    };
    let result = (|| {
        let mut inputs = inputs;
        inputs.verify(
            #[cfg(test)]
            hooks.check.as_mut().map(|check| &mut **check as _),
        )?;
        let bytes = crate::launchd_descriptor::render(owner.layout())?
            .into_bytes()
            .into_boxed_slice();
        let mut anchors = Anchors::open(owner.layout(), &[])?;
        let temporary = name(
            #[cfg(test)]
            hooks,
        )?;
        #[cfg(test)]
        hooks.at(Point::BeforeCreate)?;
        inputs.custody()?;
        anchors.check(&[])?;
        let fd = unsafe {
            libc::openat(
                anchors.parent().as_raw_fd(),
                temporary.as_ptr(),
                libc::O_RDWR
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CREAT
                    | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err("descriptor creation failed");
        }
        residue = true;
        let mut writer = unsafe { File::from_raw_fd(fd) };
        file_policy(&writer, 0o600, 0)?;
        let original = writer.metadata().map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.at(Point::Created)?;
        anchors.transition(&[&temporary], true)?;
        inputs.custody()?;
        file_policy(&writer, 0o600, 0)?;
        writer
            .write_all(&bytes)
            .map_err(|_| "descriptor write failed")?;
        #[cfg(test)]
        hooks.at(Point::Written)?;
        file_policy(&writer, 0o600, bytes.len())?;
        bytes_match(&writer, &bytes)?;
        anchors.check(&[&temporary])?;
        inputs.custody()?;
        if unsafe { libc::fchmod(writer.as_raw_fd(), 0o400) } != 0 {
            return Err("descriptor permissions failed");
        }
        #[cfg(test)]
        hooks.at(Point::Readonly)?;
        file_policy(&writer, 0o400, bytes.len())?;
        writer.sync_all().map_err(|_| "descriptor sync failed")?;
        #[cfg(test)]
        hooks.at(Point::Synced)?;
        let fingerprint = file_policy(&writer, 0o400, bytes.len())?;
        let metadata = writer.metadata().map_err(|_| UNSAFE)?;
        if (metadata.dev(), metadata.ino(), metadata.gid())
            != (original.dev(), original.ino(), original.gid())
        {
            return Err(UNSAFE);
        }
        bytes_match(&writer, &bytes)?;
        let file = open_at(anchors.parent().as_raw_fd(), &temporary, false)?;
        if file_policy(&file, 0o400, bytes.len())? != fingerprint {
            return Err(UNSAFE);
        }
        bytes_match(&file, &bytes)?;
        drop(writer);
        let mut descriptor = Descriptor {
            file,
            fingerprint,
            bytes,
        };
        #[cfg(test)]
        hooks.at(Point::BeforeRename)?;
        inputs.verify(
            #[cfg(test)]
            hooks.check.as_mut().map(|check| &mut **check as _),
        )?;
        anchors.check(&[&temporary])?;
        descriptor.check(&anchors, &temporary)?;
        #[cfg(test)]
        hooks.at(Point::BeforeRenameCall)?;
        phase = StagePhase::UncertainRename;
        if let Err(error) = rename(
            anchors.parent(),
            &temporary,
            #[cfg(test)]
            hooks,
        ) {
            if error.raw_os_error().is_some_and(|code| {
                [
                    libc::EEXIST,
                    libc::ENOENT,
                    libc::ENOTDIR,
                    libc::EACCES,
                    libc::EPERM,
                    libc::EROFS,
                    libc::EINVAL,
                    libc::EXDEV,
                ]
                .contains(&code)
            }) {
                phase = StagePhase::BeforeRename;
            }
            return Err("descriptor rename failed");
        }
        phase = StagePhase::AppliedRename;
        #[cfg(test)]
        hooks.at(Point::Renamed)?;
        anchors.transition(&[FINAL], false)?;
        descriptor.renamed(&anchors)?;
        inputs.custody()?;
        #[cfg(test)]
        hooks.at(Point::BeforeParentSync)?;
        anchors
            .parent()
            .sync_all()
            .map_err(|_| "descriptor parent sync failed")?;
        let mut value = Staged {
            inputs,
            anchors,
            descriptor,
        };
        #[cfg(test)]
        hooks.at(Point::Complete)?;
        value.revalidate(
            #[cfg(test)]
            hooks.check.as_mut().map(|check| &mut **check as _),
        )?;
        Ok(StagedDescriptor { inner: value })
    })();
    result.map_err(|reason| StageFailure {
        reason,
        phase,
        residue,
    })
}

#[cfg(test)]
pub(crate) fn revalidate(
    value: &mut StagedDescriptor<'_, '_>,
    check: Check<'_>,
) -> Result<(), &'static str> {
    value.inner.revalidate(Some(check))
}
