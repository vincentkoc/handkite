use super::*;
use journal::Journal;

pub(in crate::selection_intent) struct Pending<'held, 'lock> {
    inspection: Inspection<'held, 'lock>,
    journal: Journal,
}

impl Pending<'_, '_> {
    fn custody(&self) -> Result<(), &'static str> {
        let retained = &self.inspection.inner;
        retained.owner.revalidate()?;
        retained.snapshot.reobserve_unchanged()?;
        if let Some(previous) = &retained.previous {
            previous.revalidate_custody()?;
        }
        retained.destination.revalidate_custody()?;
        self.journal.check(retained.destination.selection_parent())
    }

    pub(crate) fn reobserve(
        &mut self,
        #[cfg(test)] check: Option<Check<'_>>,
    ) -> Result<Observation, &'static str> {
        self.custody()?;
        let observation = self.inspection.inner.reobserve(
            #[cfg(test)]
            check,
        )?;
        self.custody()?;
        Ok(observation)
    }
}

pub(crate) fn inspect_pending<'held, 'lock>(
    owner: &'lock InstallerLock,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    #[cfg(test)] check: Option<Check<'_>>,
) -> Result<(PendingInspection<'held, 'lock>, Observation), &'static str> {
    owner.revalidate()?;
    let digest = parse_target(destination.directory_name().as_bytes())?;
    if !destination.matches_owner_and_digest(owner, &digest) {
        return Err(MISMATCH);
    }
    destination.revalidate_custody()?;
    let (journal, intent) = Journal::open_existing(destination.selection_parent())?;
    let (inspection, observation) = inspect(
        owner,
        &intent,
        previous,
        destination,
        #[cfg(test)]
        check,
    )?;
    let inner = Pending {
        inspection,
        journal,
    };
    inner.custody()?;
    Ok((PendingInspection { inner }, observation))
}

#[cfg(test)]
pub(crate) fn reobserve_for_test(
    value: &mut PendingInspection<'_, '_>,
    check: Check<'_>,
) -> Result<Observation, &'static str> {
    value.inner.reobserve(Some(check))
}
