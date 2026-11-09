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

#[cfg(all(test, not(target_os = "macos")))]
mod unsupported_tests {
    use super::*;
    use crate::install_config::RetainedConfig;
    use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};

    #[test]
    fn retained_config_cannot_be_acquired_on_an_unsupported_platform() {
        // Only this private test can construct an unsupported ownership value.
        let owner = InstallerLock { _unsupported: () };
        let expected = DeviceBinding {
            key: DeviceKey::new("phone").unwrap(),
            usb_id: HardwareId::new("test-device").unwrap(),
        };
        assert_eq!(
            RetainedConfig::acquire(&owner, &expected).err(),
            Some("protected configuration verification requires macOS")
        );
    }
}
