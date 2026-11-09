//! Exclusive installer ownership of the daemon's fixed lock inode.
//!
//! Ownership alone is not stopped-job, trusted-release, or installation authority.

use crate::service_layout::ServiceLayout;

#[cfg(target_os = "macos")]
mod macos;

const UNAVAILABLE: &str = "installer lock unavailable or unsafe";

pub struct InstallerLock {
    #[cfg(target_os = "macos")]
    inner: macos::Guard,
    #[cfg(target_os = "macos")]
    layout: ServiceLayout,
    #[cfg(not(target_os = "macos"))]
    _unsupported: (),
}

impl InstallerLock {
    /// Acquire the fixed lock without truncating, replacing, or removing it.
    pub fn acquire(layout: &ServiceLayout) -> Result<Self, &'static str> {
        #[cfg(target_os = "macos")]
        {
            macos::acquire(layout).map(|inner| Self {
                inner,
                layout: layout.clone(),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = layout;
            Err(UNAVAILABLE)
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn layout(&self) -> &ServiceLayout {
        &self.layout
    }

    /// Recheck held descriptors and a fresh root-anchored path while owning the lock.
    pub fn revalidate(&self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate().map_err(|_| UNAVAILABLE)
        }
        #[cfg(not(target_os = "macos"))]
        Err(UNAVAILABLE)
    }
}
