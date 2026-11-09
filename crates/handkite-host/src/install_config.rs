//! Protected reading of the fixed service configuration for startup and installation.

use crate::service_layout::ServiceLayout;
use handkite_core::protocol::DeviceBinding;

/// Validate existing config bytes against an independently supplied device binding.
///
/// This neither observes a device nor grants installation authority. It creates
/// or repairs nothing. Success is bounded by
/// the checks performed: same-account races and replacement/restoration between
/// observations remain possible, including changes after this function returns.
pub fn verify(layout: &ServiceLayout, expected: &DeviceBinding) -> Result<(), &'static str> {
    read_protected(layout, Some(expected)).map(|_| ())
}

/// Load the configured binding after protected reading and final path revalidation.
///
/// The binding comes from the fixed config, without comparison to an independent
/// expected device. This neither observes a device nor grants installer authority.
pub fn load_protected(layout: &ServiceLayout) -> Result<DeviceBinding, &'static str> {
    read_protected(layout, None)
}

fn read_protected(
    layout: &ServiceLayout,
    expected: Option<&DeviceBinding>,
) -> Result<DeviceBinding, &'static str> {
    #[cfg(target_os = "macos")]
    {
        macos::read(
            layout,
            expected,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (layout, expected);
        Err("protected configuration verification requires macOS")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::host_config::{MAX_CONFIG_BYTES, parse};
    use crate::release_files::{
        Fingerprint, check_acl, components, directory_matches, directory_metadata, open_at,
    };
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    const UNSAFE: &str = "unsafe or changed service configuration";

    struct Directory {
        file: File,
        fingerprint: Fingerprint,
        private: bool,
        account_owned: bool,
    }

    struct AnchoredConfig {
        components: Vec<CString>,
        directories: Vec<Directory>,
        file: File,
        fingerprint: Fingerprint,
        uid: u32,
    }

    impl AnchoredConfig {
        fn open(layout: &ServiceLayout) -> Result<Self, &'static str> {
            let mut parts = components(&layout.config_file())?;
            if parts.pop().as_deref() != Some(c"host.json") {
                return Err(UNSAFE);
            }
            // Fixed suffix below home: Library/Application Support/Handkite/config.
            let home_index = parts.len().checked_sub(4).ok_or(UNSAFE)?;
            let uid = unsafe { libc::geteuid() };
            let mut directories = Vec::with_capacity(parts.len() + 1);
            let mut file = open_at(libc::AT_FDCWD, c"/", true)?;
            for index in 0..=parts.len() {
                let private = index == parts.len();
                let account_owned = index >= home_index;
                let fingerprint = directory_policy(&file, uid, private, account_owned)?;
                directories.push(Directory {
                    file,
                    fingerprint,
                    private,
                    account_owned,
                });
                if let Some(component) = parts.get(index) {
                    file = open_at(
                        directories.last().ok_or(UNSAFE)?.file.as_raw_fd(),
                        component,
                        true,
                    )?;
                } else {
                    break;
                }
            }
            let parent = &directories.last().ok_or(UNSAFE)?.file;
            let file = open_at(parent.as_raw_fd(), c"host.json", false)?;
            let fingerprint = file_policy(&file, uid)?;
            let held = Self {
                components: parts,
                directories,
                file,
                fingerprint,
                uid,
            };
            held.revalidate()?;
            Ok(held)
        }

        fn revalidate(&self) -> Result<(), &'static str> {
            let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
            for (index, held) in self.directories.iter().enumerate() {
                if index != 0 {
                    walked = open_at(walked.as_raw_fd(), &self.components[index - 1], true)?;
                }
                for file in [&held.file, &walked] {
                    let current =
                        directory_policy(file, self.uid, held.private, held.account_owned)?;
                    if !directory_matches(&current, &held.fingerprint, held.private) {
                        return Err(UNSAFE);
                    }
                }
            }
            let current = open_at(walked.as_raw_fd(), c"host.json", false)?;
            for file in [&self.file, &current] {
                if file_policy(file, self.uid)? != self.fingerprint {
                    return Err(UNSAFE);
                }
            }
            Ok(())
        }
    }

    fn directory_policy(
        file: &File,
        uid: u32,
        private: bool,
        account_owned: bool,
    ) -> Result<Fingerprint, &'static str> {
        let fingerprint = directory_metadata(file, uid, private)?;
        if account_owned && file.metadata().map_err(|_| UNSAFE)?.uid() != uid {
            return Err(UNSAFE);
        }
        Ok(fingerprint)
    }

    fn file_policy(file: &File, uid: u32) -> Result<Fingerprint, &'static str> {
        let metadata = file.metadata().map_err(|_| UNSAFE)?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
            || metadata.len() > MAX_CONFIG_BYTES
        {
            return Err(UNSAFE);
        }
        check_acl(file.as_raw_fd(), true)?;
        Ok(metadata.into())
    }

    pub(super) fn read(
        layout: &ServiceLayout,
        expected: Option<&DeviceBinding>,
        #[cfg(test)] after_read: Option<&mut dyn FnMut()>,
    ) -> Result<DeviceBinding, &'static str> {
        // Keep lower-level release helper errors inside this config boundary.
        let check = || {
            let mut held = AnchoredConfig::open(layout)?;
            let mut bytes = Vec::with_capacity(MAX_CONFIG_BYTES as usize + 1);
            (&mut held.file)
                .take(MAX_CONFIG_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| UNSAFE)?;
            #[cfg(test)]
            if let Some(after_read) = after_read {
                after_read();
            }
            if bytes.len() as u64 != held.fingerprint.size {
                return Err(UNSAFE);
            }
            held.revalidate()?;
            let binding = parse(&bytes)?;
            if let Some(expected) = expected {
                if binding.key != expected.key || binding.usb_id != expected.usb_id {
                    return Err(UNSAFE);
                }
            }
            held.revalidate()?;
            Ok(binding)
        };
        check().map_err(|_| UNSAFE)
    }

    #[cfg(test)]
    mod tests;
}
