use super::*;

/// An explicit change between two independently verified, different releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOperation {
    Upgrade,
    Rollback,
}

impl UpdateOperation {
    fn operation(self) -> Operation {
        match self {
            Self::Upgrade => Operation::Upgrade,
            Self::Rollback => Operation::Rollback,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionStage {
    Preparation,
    Commit,
    Retirement,
    Verification,
}

/// The boundary within the reported stage, not the whole selection transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionPhase {
    Before,
    Applied,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionFailure {
    pub(super) stage: SelectionStage,
    pub(super) phase: SelectionPhase,
    pub(super) has_residue: bool,
}

impl SelectionFailure {
    pub fn stage(&self) -> SelectionStage {
        self.stage
    }

    pub fn phase(&self) -> SelectionPhase {
        self.phase
    }

    /// Retirement and verification happen only after a successful selection commit.
    pub fn selection_may_be_applied(&self) -> bool {
        match self.stage {
            SelectionStage::Preparation => false,
            SelectionStage::Commit => self.phase != SelectionPhase::Before,
            SelectionStage::Retirement | SelectionStage::Verification => true,
        }
    }

    pub fn has_residue(&self) -> bool {
        self.has_residue
    }
}

/// Change an existing selection once and leave the service stopped.
///
/// Both releases and the original device agreement must retain this exact lock.
/// The previous release must match current; rollback explicitly names its destination.
/// Every phase keeps the existing signer, custody, device and absent-job guards.
/// No failure retries, restores a link, removes evidence, or activates a service.
pub async fn select_update<'held, 'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    operation: UpdateOperation,
    previous: &'held mut PublishedRelease<'lock>,
    destination: &'held mut PublishedRelease<'lock>,
) -> Result<(), SelectionFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::transition::select(
            owner,
            agreement,
            operation.operation(),
            Some(previous),
            destination,
            #[cfg(test)]
            &mut macos::transition::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            owner,
            agreement,
            operation.operation(),
            previous,
            destination,
        );
        Err(SelectionFailure {
            stage: SelectionStage::Preparation,
            phase: SelectionPhase::Before,
            has_residue: false,
        })
    }
}
