use super::StagePhase;
use crate::{
    device_agreement::DeviceAgreement, install_config::RetainedConfig,
    installer_lock::InstallerLock, release_stage::PublishedRelease,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Registered,
    VerifiedExisting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistrationFailure {
    pub(crate) phase: StagePhase,
    pub(crate) residue: bool,
}

impl RegistrationFailure {
    pub fn phase(&self) -> StagePhase {
        self.phase
    }
    pub fn has_residue(&self) -> bool {
        self.residue
    }
}

/// Publish the fixed GUI LaunchAgent or verify its existing exact bytes.
/// Requires the selected release, original device agreement and staged descriptor
/// under one installer lock. Never creates parents, starts a job, changes selection,
/// overwrites an entry, retries, or removes a failed attempt's files.
pub async fn register<'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    config: &mut RetainedConfig<'lock>,
    release: &mut PublishedRelease<'lock>,
) -> Result<RegistrationOutcome, RegistrationFailure> {
    #[cfg(target_os = "macos")]
    {
        super::staging::macos::registration::register(
            owner,
            agreement,
            config,
            release,
            #[cfg(test)]
            &mut super::staging::macos::registration::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, agreement, config, release);
        Err(RegistrationFailure {
            phase: StagePhase::BeforeRename,
            residue: false,
        })
    }
}
