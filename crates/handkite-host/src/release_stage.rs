//! Guard-bound staging and publication, without release selection or activation.

use crate::installer_lock::InstallerLock;
use std::path::Path;

#[cfg(target_os = "macos")]
mod macos;
mod publication;
pub use publication::{
    PublicationFailure, PublicationPhase, PublishedRelease, publish, reopen_published,
};

/// Retains destination descriptors, exact bounded manifest bytes, and the trust
/// inputs used for staging, while borrowing installer ownership.
/// This is not release acceptance or permission to select or execute the files.
pub struct StagedRelease<'lock> {
    _owner: &'lock InstallerLock,
    #[cfg(target_os = "macos")]
    _destination: crate::release_files::AnchoredRelease,
    #[cfg(target_os = "macos")]
    _canonical_manifest: Box<[u8]>,
    #[cfg(target_os = "macos")]
    _trust: crate::release_verify::macos::ValidatedTrust,
    directory_name: String,
}

impl StagedRelease<'_> {
    /// A relative diagnostic locator under the owner's fixed releases directory.
    pub fn directory_name(&self) -> &str {
        &self.directory_name
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StageFailure {
    reason: &'static str,
    residue: Option<String>,
}

impl StageFailure {
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// Present only after this invocation's mkdir succeeded. Never cleanup authority.
    pub fn residue_directory_name(&self) -> Option<&str> {
        self.residue.as_deref()
    }
}

/// Copy the three independently pinned signed artifacts into one new private stage.
/// No candidate executable runs; failures leave residue untouched and never retry.
pub fn stage<'lock>(
    owner: &'lock InstallerLock,
    source: &Path,
    canonical_manifest: &[u8],
    expected_digest: &[u8; 32],
    certificate_sha1: &str,
) -> Result<StagedRelease<'lock>, StageFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::stage(
            owner,
            source,
            canonical_manifest,
            expected_digest,
            certificate_sha1,
            #[cfg(test)]
            &mut macos::Hooks::production(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            owner,
            source,
            canonical_manifest,
            expected_digest,
            certificate_sha1,
        );
        Err(StageFailure {
            reason: "release staging requires macOS",
            residue: None,
        })
    }
}
