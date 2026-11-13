use super::*;
use preparation::commit::{self, retirement};

#[cfg(test)]
pub(crate) struct Hooks<'a> {
    pub(crate) preparation: preparation::Hooks<'a>,
    pub(crate) commit: commit::Hooks<'a>,
    pub(crate) retirement: retirement::Hooks<'a>,
    pub(crate) final_check: Option<Check<'a>>,
}

#[cfg(test)]
impl Hooks<'_> {
    pub(crate) fn production() -> Self {
        Self {
            preparation: preparation::Hooks::production(),
            commit: commit::Hooks::production(),
            retirement: retirement::Hooks::production(),
            final_check: None,
        }
    }
}

pub(crate) async fn select<'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    operation: Operation,
    previous: Option<&mut PublishedRelease<'lock>>,
    destination: &mut PublishedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<(), SelectionFailure> {
    let prepared = preparation::prepare(
        owner,
        agreement,
        operation,
        previous,
        destination,
        #[cfg(test)]
        &mut hooks.preparation,
    )
    .await
    .map_err(|failure| SelectionFailure {
        stage: SelectionStage::Preparation,
        phase: SelectionPhase::Before,
        has_residue: !failure.residue_candidates().is_empty(),
    })?;
    let committed = commit::commit(
        prepared,
        #[cfg(test)]
        &mut hooks.commit,
    )
    .await
    .map_err(|failure| SelectionFailure {
        stage: SelectionStage::Commit,
        phase: match failure.phase() {
            CommitPhase::BeforeCommit => SelectionPhase::Before,
            CommitPhase::AfterCommit => SelectionPhase::Applied,
            CommitPhase::UncertainCommit => SelectionPhase::Uncertain,
        },
        has_residue: !failure.residue_candidates().is_empty(),
    })?;
    let mut retired = retirement::retire(
        committed,
        #[cfg(test)]
        &mut hooks.retirement,
    )
    .await
    .map_err(|failure| SelectionFailure {
        stage: SelectionStage::Retirement,
        phase: match failure.phase() {
            RetirementPhase::Before => SelectionPhase::Before,
            RetirementPhase::Applied => SelectionPhase::Applied,
            RetirementPhase::Uncertain => SelectionPhase::Uncertain,
        },
        has_residue: !failure.residue_candidates().is_empty(),
    })?;

    let verify = || SelectionFailure {
        stage: SelectionStage::Verification,
        phase: SelectionPhase::Applied,
        has_residue: true,
    };
    owner.require_jobs_absent().map_err(|_| verify())?;
    // Recheck the retained selection after the potentially blocking job observation.
    #[cfg(test)]
    if let Some(check) = hooks.final_check.as_mut() {
        retirement::revalidate_for_test(&mut retired, &mut **check).map_err(|_| verify())?;
    } else {
        retired.revalidate().map_err(|_| verify())?;
    }
    #[cfg(not(test))]
    retired.revalidate().map_err(|_| verify())?;
    Ok(())
}
