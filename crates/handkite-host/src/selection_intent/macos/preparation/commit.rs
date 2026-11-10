use super::*;
use crate::selection_intent::{CommitFailure, CommitPhase, CommittedSelection};
use parent_change::{BeforeRename, RetainedInventory};

pub(crate) mod retirement;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    BeforePreflight,
    BeforeRename,
    BeforeRenameCall,
    Renamed,
    BeforeSync,
    BeforeFinalVerify,
    Complete,
}

#[cfg(test)]
pub(crate) struct Hooks<'a> {
    pub(crate) check: Option<Check<'a>>,
    pub(crate) device: Option<crate::device_agreement::tests::Fake>,
    pub(crate) fault: Option<&'a mut dyn FnMut(Point) -> Result<(), &'static str>>,
    pub(crate) rename_error: Option<(i32, bool)>,
    pub(crate) sync_error: bool,
    pub(crate) renames: usize,
}

#[cfg(test)]
impl Hooks<'_> {
    pub(crate) fn production() -> Self {
        Self {
            check: None,
            device: None,
            fault: None,
            rename_error: None,
            sync_error: false,
            renames: 0,
        }
    }

    fn at(&mut self, point: Point) -> Result<(), &'static str> {
        if let Some(fault) = &mut self.fault {
            fault(point)?;
        }
        Ok(())
    }
}

pub(in crate::selection_intent) struct Committed<'held, 'lock> {
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    selected: RetainedLink,
    previous_link: Option<RetainedLink>,
    candidate: CString,
    journal: Journal,
    inventory: RetainedInventory,
}

impl Committed<'_, '_> {
    fn custody(&mut self) -> Result<(), &'static str> {
        self.owner.revalidate()?;
        self.agreement.revalidate_for_owner(self.owner)?;
        let parent = self.destination.selection_parent();
        self.inventory.check(parent)?;
        self.selected.check(parent)?;
        if let Some(previous) = &self.previous_link {
            previous.check(parent)?;
        } else {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    self.candidate.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
                || io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
            {
                return Err(UNSAFE);
            }
        }
        self.journal.check(parent)?;
        if let Some(previous) = &self.previous {
            previous.revalidate_custody()?;
        }
        self.destination.revalidate_custody()?;
        self.owner.revalidate()?;
        self.agreement.revalidate_for_owner(self.owner)?;
        self.inventory.check(self.destination.selection_parent())
    }

    pub(crate) fn revalidate(
        &mut self,
        #[cfg(test)] mut check: Option<Check<'_>>,
    ) -> Result<(), &'static str> {
        self.custody()?;
        for release in self
            .previous
            .iter_mut()
            .map(|value| &mut **value)
            .chain([&mut *self.destination])
        {
            #[cfg(test)]
            if let Some(check) = &mut check {
                check(release)?;
            } else {
                release.revalidate()?;
            }
            #[cfg(not(test))]
            release.revalidate()?;
        }
        self.custody()
    }
}

fn native_rename(parent: &File, candidate: &CStr, exchange: bool) -> io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            candidate.as_ptr(),
            parent.as_raw_fd(),
            c"current".as_ptr(),
            if exchange {
                libc::RENAME_SWAP
            } else {
                libc::RENAME_EXCL
            },
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename(
    parent: &File,
    candidate: &CStr,
    exchange: bool,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> io::Result<()> {
    #[cfg(test)]
    {
        hooks.renames += 1;
        if let Some((errno, applied)) = hooks.rename_error {
            if applied {
                native_rename(parent, candidate, exchange)?;
            }
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
    native_rename(parent, candidate, exchange)
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

pub(crate) async fn commit<'held, 'lock>(
    prepared: PreparedSelection<'held, 'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<CommittedSelection<'held, 'lock>, CommitFailure> {
    let mut prepared = prepared.inner;
    let candidate = prepared.link.name().to_owned();
    let residues = vec![
        candidate.to_string_lossy().into_owned(),
        "current".into(),
        PENDING_JOURNAL.into(),
    ];
    let mut phase = CommitPhase::BeforeCommit;
    let result = async {
        #[cfg(test)]
        hooks.at(Point::BeforePreflight)?;
        #[cfg(test)]
        prepared.revalidate(hooks.check.as_mut().map(|check| &mut **check as Check<'_>))?;
        #[cfg(not(test))]
        prepared.revalidate()?;
        #[cfg(test)]
        if let Some(transport) = hooks.device.take() {
            crate::device_agreement::tests::reobserve(&mut prepared.agreement, transport).await?;
        } else {
            prepared.agreement.reobserve().await?;
        }
        #[cfg(not(test))]
        prepared.agreement.reobserve().await?;
        // Check every retained baseline after the await, before inventory or rename.
        prepared.custody()?;
        checkpoint(
            &prepared.retained,
            &mut prepared.agreement,
            Some(&prepared.link),
        )?;
        finish_commit(
            prepared,
            candidate,
            &mut phase,
            #[cfg(test)]
            hooks,
        )
    }
    .await;
    result.map_err(|reason| CommitFailure {
        reason,
        phase,
        residues,
    })
}

// Inventory capture, rename, durability checks and returned custody never suspend.
fn finish_commit<'held, 'lock>(
    mut prepared: Prepared<'held, 'lock>,
    candidate: CString,
    phase: &mut CommitPhase,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<CommittedSelection<'held, 'lock>, &'static str> {
    let before = BeforeRename::capture(
        prepared.retained.destination.selection_parent(),
        &prepared.link,
        prepared.previous_link.as_ref(),
    )?;
    #[cfg(test)]
    hooks.at(Point::BeforeRename)?;
    prepared.custody()?;
    checkpoint(
        &prepared.retained,
        &mut prepared.agreement,
        Some(&prepared.link),
    )?;
    before.check(prepared.retained.destination.selection_parent())?;
    #[cfg(test)]
    hooks.at(Point::BeforeRenameCall)?;
    if let Err(error) = rename(
        prepared.retained.destination.selection_parent(),
        &candidate,
        prepared.previous_link.is_some(),
        #[cfg(test)]
        hooks,
    ) {
        if !known_refusal(&error) {
            *phase = CommitPhase::UncertainCommit;
        }
        return Err("selection rename failed");
    }
    // Never let a later observation, signer or sync failure erase this boundary.
    *phase = CommitPhase::AfterCommit;
    #[cfg(test)]
    hooks.at(Point::Renamed)?;
    let (change, inventory) = before.finish(prepared.retained.destination.selection_parent())?;
    prepared.link.rebind_after_rename(
        prepared.retained.destination.selection_parent(),
        c"current".into(),
        &change,
    )?;
    if let Some(previous) = &mut prepared.previous_link {
        previous.rebind_after_rename(
            prepared.retained.destination.selection_parent(),
            candidate.clone(),
            &change,
        )?;
    }
    prepared
        .agreement
        .revalidate_for_owner(prepared.retained.owner)?;
    prepared.retained.owner.revalidate()?;
    if let Some(previous) = &mut prepared.retained.previous {
        previous.selection_parent_transition(&change)?;
    }
    prepared
        .retained
        .destination
        .selection_parent_transition(&change)?;
    let mut committed = Committed {
        owner: prepared.retained.owner,
        agreement: prepared.agreement,
        previous: prepared.retained.previous,
        destination: prepared.retained.destination,
        selected: prepared.link,
        previous_link: prepared.previous_link,
        candidate,
        journal: prepared.journal,
        inventory,
    };
    committed.custody()?;
    #[cfg(test)]
    hooks.at(Point::BeforeSync)?;
    #[cfg(test)]
    if hooks.sync_error {
        return Err("selection parent sync failed");
    }
    committed
        .destination
        .selection_parent()
        .sync_all()
        .map_err(|_| "selection parent sync failed")?;
    #[cfg(test)]
    hooks.at(Point::BeforeFinalVerify)?;
    #[cfg(test)]
    committed.revalidate(hooks.check.as_mut().map(|check| &mut **check as Check<'_>))?;
    #[cfg(not(test))]
    committed.revalidate()?;
    #[cfg(test)]
    hooks.at(Point::Complete)?;
    committed.custody()?;
    Ok(CommittedSelection { inner: committed })
}

#[cfg(test)]
pub(crate) fn revalidate_for_test(
    value: &mut CommittedSelection<'_, '_>,
    check: Check<'_>,
) -> Result<(), &'static str> {
    value.inner.revalidate(Some(check))
}
