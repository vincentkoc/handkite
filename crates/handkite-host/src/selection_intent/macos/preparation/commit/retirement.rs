use super::*;
use crate::selection_intent::{RetiredSelection, RetirementFailure, RetirementPhase};
use parent_change::BeforeRetirement;

#[cfg(test)]
pub(crate) use super::{Hooks, Point};

fn native_rename(parent: &File, completion: &CStr) -> io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            JOURNAL.as_ptr(),
            parent.as_raw_fd(),
            completion.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename(parent: &File, completion: &CStr, #[cfg(test)] hooks: &mut Hooks<'_>) -> io::Result<()> {
    #[cfg(test)]
    {
        hooks.renames += 1;
        if let Some((errno, applied)) = hooks.rename_error {
            if applied {
                native_rename(parent, completion)?;
            }
            return Err(io::Error::from_raw_os_error(errno));
        }
    }
    native_rename(parent, completion)
}

pub(crate) async fn retire<'held, 'lock>(
    committed: CommittedSelection<'held, 'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<RetiredSelection<'held, 'lock>, RetirementFailure> {
    let mut committed = committed.inner;
    let mut residues = vec![
        PENDING_JOURNAL.into(),
        "current".into(),
        committed.candidate.to_string_lossy().into_owned(),
    ];
    let mut phase = RetirementPhase::Before;
    let result = async {
        let completion = committed.journal.completion_name()?;
        residues.push(completion.to_string_lossy().into_owned());
        #[cfg(test)]
        hooks.at(Point::BeforePreflight)?;
        #[cfg(test)]
        committed.revalidate(hooks.check.as_mut().map(|check| &mut **check as Check<'_>))?;
        #[cfg(not(test))]
        committed.revalidate()?;
        #[cfg(test)]
        if let Some(transport) = hooks.device.take() {
            crate::device_agreement::tests::reobserve(&mut committed.agreement, transport).await?;
        } else {
            committed.agreement.reobserve().await?;
        }
        #[cfg(not(test))]
        committed.agreement.reobserve().await?;
        committed.owner.require_user_job_absent()?;
        // Retain all original evidence across the await; never adopt a fresh baseline.
        committed.custody()?;
        finish_retirement(
            committed,
            completion,
            &mut phase,
            #[cfg(test)]
            hooks,
        )
    }
    .await;
    result.map_err(|reason| RetirementFailure {
        reason,
        phase,
        residues,
    })
}

// Nothing below this boundary can suspend, retry, overwrite, unlink, or restore.
fn finish_retirement<'held, 'lock>(
    mut committed: Committed<'held, 'lock>,
    completion: CString,
    phase: &mut RetirementPhase,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<RetiredSelection<'held, 'lock>, &'static str> {
    let before = BeforeRetirement::capture(
        committed.destination.selection_parent(),
        &committed.journal,
        completion.clone(),
    )?;
    #[cfg(test)]
    hooks.at(Point::BeforeRename)?;
    committed.custody()?;
    before.check(committed.destination.selection_parent())?;
    #[cfg(test)]
    hooks.at(Point::BeforeRenameCall)?;
    if let Err(error) = rename(
        committed.destination.selection_parent(),
        &completion,
        #[cfg(test)]
        hooks,
    ) {
        if !known_refusal(&error) {
            *phase = RetirementPhase::Uncertain;
        }
        return Err("selection journal retirement rename failed");
    }
    *phase = RetirementPhase::Applied;
    #[cfg(test)]
    hooks.at(Point::Renamed)?;
    let (change, inventory) = before.finish(committed.destination.selection_parent())?;
    committed.journal.rebind_after_rename(
        committed.destination.selection_parent(),
        completion,
        &change,
    )?;
    committed.agreement.revalidate_for_owner(committed.owner)?;
    committed.owner.revalidate()?;
    if let Some(previous) = &mut committed.previous {
        previous.selection_parent_transition(&change)?;
    }
    committed.destination.selection_parent_transition(&change)?;
    committed.inventory = inventory;
    committed.custody()?;
    #[cfg(test)]
    hooks.at(Point::BeforeSync)?;
    #[cfg(test)]
    if hooks.sync_error {
        return Err("selection journal retirement parent sync failed");
    }
    committed
        .destination
        .selection_parent()
        .sync_all()
        .map_err(|_| "selection journal retirement parent sync failed")?;
    #[cfg(test)]
    hooks.at(Point::BeforeFinalVerify)?;
    #[cfg(test)]
    committed.revalidate(hooks.check.as_mut().map(|check| &mut **check as Check<'_>))?;
    #[cfg(not(test))]
    committed.revalidate()?;
    #[cfg(test)]
    hooks.at(Point::Complete)?;
    committed.custody()?;
    Ok(RetiredSelection { inner: committed })
}

#[cfg(test)]
pub(crate) fn revalidate_for_test(
    value: &mut RetiredSelection<'_, '_>,
    check: Check<'_>,
) -> Result<(), &'static str> {
    value.inner.revalidate(Some(check))
}
