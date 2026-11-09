use super::{PublicationFailure, PublicationPhase, PublishedRelease, StagedRelease};
use crate::current_selection::{encode_target, macos::Snapshot};
use crate::release_files::{Fingerprint, directory_matches};
use crate::release_manifest::ArtifactName;
use crate::release_verify::macos::{rehash_held, verify_held};
use ring::rand::{SecureRandom, SystemRandom};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(test)]
use std::path::Path;

mod receipt;
mod retained;
pub(super) use receipt::Receipt;
pub(super) use retained::{check_pair, reopen_published, revalidate};

const UNSAFE: &str = "unsafe or changed publication files";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rename {
    Receipt,
    Stage,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    BeforeCreate,
    BeforeCreateCall,
    ReceiptCreated,
    BeforeWrite,
    ReceiptWritten,
    BeforeChmod,
    ReceiptReadonly,
    BeforeReceiptSync,
    ReceiptClosed,
    BeforeReceiptRename,
    BeforeReceiptRenameCall,
    ReceiptRenamed,
    BeforeReceiptParentSync,
    BeforeStageRename,
    BeforeStageRenameCall,
    StageRenamed,
    BeforeFinalVerify,
    AfterFinalVerify,
    BeforeDirectorySync,
    BeforeFinalParentSync,
    Complete,
}

#[cfg(test)]
type Check<'a> = &'a mut dyn FnMut(ArtifactName, &Path) -> Result<(), &'static str>;
#[cfg(test)]
type Fault<'a> = &'a mut dyn FnMut(Point, &Path) -> Result<(), &'static str>;

#[cfg(test)]
pub(super) struct Hooks<'a> {
    check: Option<Check<'a>>,
    fault: Option<Fault<'a>>,
    nonce: Option<[u8; 16]>,
    rename_error: Option<(Rename, i32, bool)>,
    renames: Vec<Rename>,
}

#[cfg(test)]
impl Hooks<'_> {
    pub(super) fn production() -> Self {
        Self {
            check: None,
            fault: None,
            nonce: None,
            rename_error: None,
            renames: Vec::new(),
        }
    }
    fn at(&mut self, point: Point, directory: &Path) -> Result<(), &'static str> {
        if let Some(fault) = self.fault.as_mut() {
            fault(point, directory)?;
        }
        Ok(())
    }
}

fn verify(
    staged: &mut StagedRelease<'_>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<(), &'static str> {
    #[cfg(test)]
    if let Some(check) = hooks.check.as_mut() {
        return verify_held(&mut staged._destination, &staged._trust, Some(&mut **check));
    }
    verify_held(
        &mut staged._destination,
        &staged._trust,
        #[cfg(test)]
        None,
    )
}

fn checkpoint(
    staged: &StagedRelease<'_>,
    snapshot: &Snapshot,
    receipt: Option<&Receipt>,
) -> Result<(), &'static str> {
    staged._owner.revalidate()?;
    snapshot.reobserve_unchanged()?;
    staged._destination.revalidate()?;
    if let Some(receipt) = receipt {
        receipt.check(
            staged._destination.publication_parent(),
            &staged._canonical_manifest,
        )?;
    }
    Ok(())
}

fn absent(parent: &File, name: &CStr) -> Result<(), &'static str> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Err("publication destination already exists");
    }
    if io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
        return Err("publication destination cannot be observed");
    }
    Ok(())
}

fn temporary_name(#[cfg(test)] hooks: &Hooks<'_>) -> Result<String, &'static str> {
    let mut nonce = [0; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "receipt name unavailable")?;
    #[cfg(test)]
    if let Some(fixed) = hooks.nonce {
        nonce = fixed;
    }
    Ok(format!(
        ".receipt-{}",
        nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn native_rename(parent: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename(
    parent: &File,
    from: &CStr,
    to: &CStr,
    which: Rename,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> io::Result<()> {
    #[cfg(test)]
    {
        hooks.renames.push(which);
        if let Some((at, errno, applied)) = hooks.rename_error {
            if at == which {
                if applied {
                    native_rename(parent, from, to)?;
                }
                return Err(io::Error::from_raw_os_error(errno));
            }
        }
    }
    #[cfg(not(test))]
    let _ = which;
    native_rename(parent, from, to)
}

fn known_refusal(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        [
            libc::EEXIST,
            libc::ENOENT,
            libc::ENOTDIR,
            libc::EISDIR,
            libc::EINVAL,
            libc::ENOTSUP,
            libc::EOPNOTSUPP,
            libc::EXDEV,
            libc::EPERM,
            libc::EACCES,
            libc::EROFS,
            libc::ENOSYS,
            libc::ENOTEMPTY,
        ]
        .contains(&code)
    })
}

pub(super) fn publish<'lock>(
    mut staged: StagedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<PublishedRelease<'lock>, PublicationFailure> {
    let mut phase = PublicationPhase::BeforeCommit;
    let mut directories = vec![staged.directory_name.clone()];
    let mut receipts = Vec::new();
    let result = (|| {
        if staged._canonical_manifest.len() > crate::release_manifest::MAX_MANIFEST_BYTES
            || staged._trust.manifest().canonical_json()?.as_slice()
                != staged._canonical_manifest.as_ref()
        {
            return Err(UNSAFE);
        }
        let digest: [u8; 32] =
            ring::digest::digest(&ring::digest::SHA256, &staged._canonical_manifest)
                .as_ref()
                .try_into()
                .map_err(|_| UNSAFE)?;
        let final_name = String::from_utf8(encode_target(&digest).to_vec()).map_err(|_| UNSAFE)?;
        let receipt_name = format!("{final_name}.manifest.json");
        let source_c = CString::new(staged.directory_name.as_str()).map_err(|_| UNSAFE)?;
        let final_c = CString::new(final_name.as_str()).map_err(|_| UNSAFE)?;
        let receipt_c = CString::new(receipt_name.as_str()).map_err(|_| UNSAFE)?;
        let directory = staged._owner.layout().releases_directory();
        if staged._destination.path(ArtifactName::Daemon)
            != directory
                .join(&staged.directory_name)
                .join(ArtifactName::Daemon.filename())
        {
            return Err(UNSAFE);
        }
        staged._owner.revalidate()?;
        let snapshot = Snapshot::capture(staged._owner.layout())?;
        if snapshot.selected_digest() == Some(digest) {
            return Err("release is already selected");
        }
        staged._destination.publication_refresh_parent()?;
        absent(staged._destination.publication_parent(), &final_c)?;
        absent(staged._destination.publication_parent(), &receipt_c)?;
        verify(
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        let temporary = temporary_name(
            #[cfg(test)]
            hooks,
        )?;
        let temporary_c = CString::new(temporary.as_str()).map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.at(Point::BeforeCreate, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        absent(staged._destination.publication_parent(), &final_c)?;
        absent(staged._destination.publication_parent(), &receipt_c)?;
        #[cfg(test)]
        hooks.at(Point::BeforeCreateCall, &directory)?;
        let fd = unsafe {
            libc::openat(
                staged._destination.publication_parent().as_raw_fd(),
                temporary_c.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            if !known_refusal(&io::Error::last_os_error()) {
                receipts.push(temporary);
            }
            return Err("publication receipt creation failed");
        }
        let mut writer = unsafe { File::from_raw_fd(fd) };
        receipts.push(temporary.clone());
        #[cfg(test)]
        hooks.at(Point::ReceiptCreated, &directory)?;
        let initial = receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o600,
            &[],
        )?;
        staged._destination.publication_refresh_parent()?;
        checkpoint(&staged, &snapshot, None)?;
        #[cfg(test)]
        hooks.at(Point::BeforeWrite, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        if receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o600,
            &[],
        )? != initial
        {
            return Err(UNSAFE);
        }
        writer
            .write_all(&staged._canonical_manifest)
            .map_err(|_| "publication receipt write failed")?;
        #[cfg(test)]
        hooks.at(Point::ReceiptWritten, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        let written = receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o600,
            &staged._canonical_manifest,
        )?;
        if !directory_matches(&written, &initial, false) {
            return Err(UNSAFE);
        }
        #[cfg(test)]
        hooks.at(Point::BeforeChmod, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        if receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o600,
            &staged._canonical_manifest,
        )? != written
        {
            return Err(UNSAFE);
        }
        if unsafe { libc::fchmod(writer.as_raw_fd(), 0o400) } != 0 {
            return Err("publication receipt mode change failed");
        }
        #[cfg(test)]
        hooks.at(Point::ReceiptReadonly, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o400,
            &staged._canonical_manifest,
        )?;
        #[cfg(test)]
        hooks.at(Point::BeforeReceiptSync, &directory)?;
        checkpoint(&staged, &snapshot, None)?;
        receipt::check_named(
            &writer,
            staged._destination.publication_parent(),
            &temporary_c,
            0o400,
            &staged._canonical_manifest,
        )?;
        writer
            .sync_all()
            .map_err(|_| "publication receipt sync failed")?;
        checkpoint(&staged, &snapshot, None)?;
        let mut receipt = Receipt::reopen(
            writer,
            staged._destination.publication_parent(),
            temporary_c.clone(),
            &staged._canonical_manifest,
        )?;
        #[cfg(test)]
        hooks.at(Point::ReceiptClosed, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeReceiptRename, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        absent(staged._destination.publication_parent(), &receipt_c)?;
        absent(staged._destination.publication_parent(), &final_c)?;
        #[cfg(test)]
        hooks.at(Point::BeforeReceiptRenameCall, &directory)?;
        if let Err(error) = rename(
            staged._destination.publication_parent(),
            &temporary_c,
            &receipt_c,
            Rename::Receipt,
            #[cfg(test)]
            hooks,
        ) {
            if !known_refusal(&error) {
                receipts.push(receipt_name.clone());
            }
            return Err("publication receipt rename failed");
        }
        receipts = vec![receipt_name.clone()];
        #[cfg(test)]
        hooks.at(Point::ReceiptRenamed, &directory)?;
        staged._owner.revalidate()?;
        snapshot.reobserve_unchanged()?;
        receipt.rebind(
            staged._destination.publication_parent(),
            receipt_c.clone(),
            &staged._canonical_manifest,
        )?;
        staged._destination.publication_refresh_parent()?;
        absent(staged._destination.publication_parent(), &temporary_c)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeReceiptParentSync, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        staged
            ._destination
            .publication_parent()
            .sync_all()
            .map_err(|_| "publication parent sync failed")?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeStageRename, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        absent(staged._destination.publication_parent(), &final_c)?;
        #[cfg(test)]
        hooks.at(Point::BeforeStageRenameCall, &directory)?;
        phase = PublicationPhase::UncertainCommit;
        directories.push(final_name.clone());
        if let Err(error) = rename(
            staged._destination.publication_parent(),
            &source_c,
            &final_c,
            Rename::Stage,
            #[cfg(test)]
            hooks,
        ) {
            if known_refusal(&error) {
                phase = PublicationPhase::BeforeCommit;
                directories.truncate(1);
            }
            return Err("publication stage rename failed");
        }
        phase = PublicationPhase::AfterCommit;
        directories = vec![final_name.clone()];
        #[cfg(test)]
        hooks.at(Point::StageRenamed, &directory)?;
        staged._owner.revalidate()?;
        snapshot.reobserve_unchanged()?;
        staged._destination.publication_rebind(&digest)?;
        staged.directory_name = final_name;
        absent(staged._destination.publication_parent(), &source_c)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeFinalVerify, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        verify(
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        #[cfg(test)]
        hooks.at(Point::AfterFinalVerify, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeDirectorySync, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        staged
            ._destination
            .publication_directory()
            .sync_all()
            .map_err(|_| "published directory sync failed")?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        #[cfg(test)]
        hooks.at(Point::BeforeFinalParentSync, &directory)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        staged
            ._destination
            .publication_parent()
            .sync_all()
            .map_err(|_| "publication parent sync failed")?;
        #[cfg(test)]
        hooks.at(Point::Complete, &directory)?;
        rehash_held(&mut staged._destination, &staged._trust)?;
        checkpoint(&staged, &snapshot, Some(&receipt))?;
        Ok(PublishedRelease {
            staged,
            _receipt: receipt,
            receipt_name,
        })
    })();
    result.map_err(|reason| PublicationFailure {
        reason,
        phase,
        directories,
        receipts,
    })
}

#[cfg(test)]
mod tests;
