use super::*;

#[cfg(test)]
pub(crate) use super::transition::Hooks;

pub(crate) async fn select<'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    destination: &mut PublishedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<(), InitialSelectionFailure> {
    super::transition::select(
        owner,
        agreement,
        Operation::InitialInstall,
        None,
        destination,
        #[cfg(test)]
        hooks,
    )
    .await
    .map_err(|failure| InitialSelectionFailure {
        stage: match failure.stage() {
            SelectionStage::Preparation => InitialSelectionStage::Preparation,
            SelectionStage::Commit => InitialSelectionStage::Commit,
            SelectionStage::Retirement => InitialSelectionStage::Retirement,
            SelectionStage::Verification => InitialSelectionStage::Verification,
        },
        selection_may_be_applied: failure.selection_may_be_applied(),
        has_residue: failure.has_residue(),
    })
}
