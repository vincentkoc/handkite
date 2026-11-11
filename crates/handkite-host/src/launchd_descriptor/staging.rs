use crate::{
    install_config::RetainedConfig, installer_lock::InstallerLock, release_stage::PublishedRelease,
};

#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// The boundary is the single exclusive rename to the fixed descriptor name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagePhase {
    BeforeRename,
    AppliedRename,
    UncertainRename,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StageFailure {
    reason: &'static str,
    phase: StagePhase,
    residue: bool,
}

impl StageFailure {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
    pub fn phase(&self) -> StagePhase {
        self.phase
    }
    /// This attempt created a file. Its name or existence is not cleanup authority.
    pub fn has_residue(&self) -> bool {
        self.residue
    }
}

/// Retains the staged bytes, filesystem anchors, and original protected inputs.
/// This handle grants no release selection or service activation authority.
pub struct StagedDescriptor<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::Staged<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported: std::marker::PhantomData<(
        &'lock InstallerLock,
        &'held mut RetainedConfig<'lock>,
        &'held mut PublishedRelease<'lock>,
    )>,
}

impl StagedDescriptor<'_, '_> {
    /// Read-only revalidation; never accepts replacement files or resets baselines.
    pub fn revalidate(&mut self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        Err("service descriptor staging requires macOS")
    }
}

/// Stage the renderer's exact bytes in the empty fixed launchd directory.
/// All inputs must borrow this same lock. Config must include a valid runner ID.
/// No overwrite, adoption, retry, cleanup, device query, or activation is performed.
pub fn stage<'held, 'lock>(
    owner: &'lock InstallerLock,
    config: &'held mut RetainedConfig<'lock>,
    release: &'held mut PublishedRelease<'lock>,
) -> Result<StagedDescriptor<'held, 'lock>, StageFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::stage(
            owner,
            config,
            release,
            #[cfg(test)]
            &mut macos::Hooks::production(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, config, release);
        Err(StageFailure {
            reason: "service descriptor staging requires macOS",
            phase: StagePhase::BeforeRename,
            residue: false,
        })
    }
}
