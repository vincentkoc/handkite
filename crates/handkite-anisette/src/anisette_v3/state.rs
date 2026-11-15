//! Fixed Mac-side reservation. Dropping a handle never removes or resets state.

use super::{AnisetteOrigin, MacosProvisionedState};
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
    /// The attempt record binds the reservation to the configured `origin`.
    pub fn reserve(origin: &AnisetteOrigin) -> Result<Self, &'static str> {
        let layout = StateLayout::current_account()
            .map_err(|_| "Anisette service account is unavailable")?;
        Self::reserve_at(&layout, origin)
    }

    fn reserve_at(layout: &StateLayout, origin: &AnisetteOrigin) -> Result<Self, &'static str> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                _sealed: (),
                inner: macos::reserve(layout, origin)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (layout, origin);
            Err("Anisette state custody requires macOS")
        }
    }

    pub(super) fn origin(&self) -> &AnisetteOrigin {
        #[cfg(target_os = "macos")]
        {
            self.inner.origin()
        }
        #[cfg(not(target_os = "macos"))]
        unreachable!("reservation is unavailable on this platform")
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
/// State reserved for a different origin is rejected.
pub fn load_ready(origin: &AnisetteOrigin) -> Result<MacosProvisionedState, &'static str> {
    let layout =
        StateLayout::current_account().map_err(|_| "Anisette service account is unavailable")?;
    #[cfg(target_os = "macos")]
    {
        macos::load(&layout, origin)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (layout, origin);
        Err("Anisette state custody requires macOS")
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod tests;
