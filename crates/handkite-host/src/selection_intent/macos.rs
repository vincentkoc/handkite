use super::*;
use crate::current_selection::macos::Snapshot;
use std::ffi::CStr;

mod journal;
pub(crate) mod preparation;

const MISMATCH: &str = "selection intent release or lock mismatch";
const UNSAFE: &str = "unsafe or changed selection preparation";
const JOURNAL: &CStr = c".selection-pending.json";

#[cfg(test)]
pub(crate) type Check<'a> =
    &'a mut dyn FnMut(&mut PublishedRelease<'_>) -> Result<(), &'static str>;

pub(super) struct Retained<'held, 'lock> {
    owner: &'lock InstallerLock,
    intent: Intent,
    snapshot: Snapshot,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
}

impl Retained<'_, '_> {
    pub(super) fn reobserve(
        &mut self,
        #[cfg(test)] mut check: Option<Check<'_>>,
    ) -> Result<Observation, &'static str> {
        self.owner.revalidate()?;
        self.snapshot.reobserve_unchanged()?;
        for release in self
            .previous
            .iter_mut()
            .map(|value| &mut **value)
            .chain([&mut *self.destination])
        {
            #[cfg(test)]
            if let Some(check) = check.as_mut() {
                check(release)?;
            } else {
                release.revalidate()?;
            }
            #[cfg(not(test))]
            release.revalidate()?;
        }
        // A later signer must not hide changes to an earlier verified pair.
        if let Some(previous) = &self.previous {
            previous.revalidate_custody()?;
        }
        self.destination.revalidate_custody()?;
        self.snapshot.reobserve_unchanged()?;
        self.owner.revalidate()?;
        let Some(manifest) = self.snapshot.selected_digest() else {
            return Ok(if self.intent.previous.is_none() {
                Observation::PreviousSelection
            } else {
                Observation::IndeterminateConflict
            });
        };
        let Some((device, inode)) = self.snapshot.selected_link_identity() else {
            return Ok(Observation::IndeterminateConflict);
        };
        let observed = LinkClaim {
            manifest,
            device,
            inode,
        };
        Ok(if self.intent.previous == Some(observed) {
            Observation::PreviousSelection
        } else if self.intent.destination == observed {
            Observation::DestinationSelection
        } else {
            Observation::IndeterminateConflict
        })
    }
}

pub(crate) fn inspect<'held, 'lock>(
    owner: &'lock InstallerLock,
    intent: &Intent,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    #[cfg(test)] check: Option<Check<'_>>,
) -> Result<(Inspection<'held, 'lock>, Observation), &'static str> {
    owner.revalidate()?;
    if !destination.matches_owner_and_digest(owner, &intent.destination.manifest) {
        return Err(MISMATCH);
    }
    match (intent.previous, &previous) {
        (None, None) => {}
        (Some(claim), Some(release))
            if release.matches_owner_and_digest(owner, &claim.manifest) => {}
        _ => return Err(MISMATCH),
    }
    let mut inner = Retained {
        owner,
        intent: intent.clone(),
        snapshot: Snapshot::capture(owner.layout())
            .map_err(|_| "indeterminate selection conflict")?,
        previous,
        destination,
    };
    let observation = inner.reobserve(
        #[cfg(test)]
        check,
    )?;
    Ok((Inspection { inner }, observation))
}

#[cfg(test)]
pub(crate) fn reobserve_for_test(
    value: &mut Inspection<'_, '_>,
    check: Check<'_>,
) -> Result<Observation, &'static str> {
    value.inner.reobserve(Some(check))
}
