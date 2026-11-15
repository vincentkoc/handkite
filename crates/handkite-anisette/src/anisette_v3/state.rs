//! Fixed Mac-side reservation. Dropping a handle never removes or resets state.

use super::MacosProvisionedState;
use crate::layout::StateLayout;

#[cfg(target_os = "macos")]
mod macos;

/// One durable reservation, consumed even if provisioning never starts.
/// No clone, alternate root, reset, retry, or pending-state recovery is exposed.
pub struct PendingProvisioning {
    _sealed: (),
    #[cfg(target_os = "macos")]
    inner: macos::Pending,
}

impl PendingProvisioning {
    /// Exclusively reserve the fixed account location before any network work.
    /// Every existing entry blocks reservation, including empty or invalid state.
    pub fn reserve() -> Result<Self, &'static str> {
        let layout = StateLayout::current_account()
            .map_err(|_| "Anisette service account is unavailable")?;
        Self::reserve_at(&layout)
    }

    fn reserve_at(layout: &StateLayout) -> Result<Self, &'static str> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                _sealed: (),
                inner: macos::reserve(layout)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = layout;
            Err("Anisette state custody requires macOS")
        }
    }

    pub(super) fn identifier(&self) -> &[u8; 16] {
        #[cfg(target_os = "macos")]
        {
            self.inner.identifier()
        }
        #[cfg(not(target_os = "macos"))]
        unreachable!("reservation is unavailable on this platform")
    }

    pub(super) fn revalidate(&self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate_pending()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("Anisette state custody requires macOS")
        }
    }

    /// Synchronous through durable publication and final path revalidation.
    pub(super) fn persist(self, state: &MacosProvisionedState) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.persist(state)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = state;
            Err("Anisette state custody requires macOS")
        }
    }
}

/// Restart can read valid ready state only; it cannot resume an attempt.
pub fn load_ready() -> Result<MacosProvisionedState, &'static str> {
    let layout =
        StateLayout::current_account().map_err(|_| "Anisette service account is unavailable")?;
    #[cfg(target_os = "macos")]
    {
        macos::load(&layout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = layout;
        Err("Anisette state custody requires macOS")
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod tests;
