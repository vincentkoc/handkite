//! Exclusive installer ownership of the daemon's fixed lock inode.
//!
//! Ownership alone is not stopped-job, trusted-release, or installation authority.

use crate::service_layout::ServiceLayout;

#[cfg(target_os = "macos")]
mod macos;

const UNAVAILABLE: &str = "installer lock unavailable or unsafe";
#[cfg(target_os = "macos")]
const JOB_UNAVAILABLE: &str = "Handkite user-domain job is loaded or unknown";

pub struct InstallerLock {
    #[cfg(target_os = "macos")]
    inner: macos::Guard,
    #[cfg(target_os = "macos")]
    layout: ServiceLayout,
    #[cfg(all(test, target_os = "macos"))]
    user_job: std::cell::Cell<Option<crate::launchd_observation::Observation>>,
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
                #[cfg(test)]
                user_job: std::cell::Cell::new(None),
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

    /// Observe the fixed user-domain job while retaining exclusive lock ownership.
    /// This does not exclude controllers in other domains or those ignoring the lock.
    #[cfg(target_os = "macos")]
    pub(crate) fn require_user_job_absent(&self) -> Result<(), &'static str> {
        self.check_user_job_absent(|| {
            #[cfg(test)]
            if let Some(observation) = self.user_job.get() {
                return observation;
            }
            crate::launchd_observation::observe()
        })
    }

    #[cfg(target_os = "macos")]
    fn check_user_job_absent(
        &self,
        observe: impl FnOnce() -> crate::launchd_observation::Observation,
    ) -> Result<(), &'static str> {
        self.revalidate()?;
        let observation = observe();
        self.revalidate()?;
        if observation != crate::launchd_observation::Observation::Absent {
            return Err(JOB_UNAVAILABLE);
        }
        Ok(())
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn set_user_job_for_test(
        &self,
        observation: crate::launchd_observation::Observation,
    ) {
        self.user_job.set(Some(observation));
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
