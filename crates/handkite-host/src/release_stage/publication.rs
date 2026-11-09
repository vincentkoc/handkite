use super::{InstallerLock, StagedRelease};

#[cfg(target_os = "macos")]
mod macos;

/// The commit point is the exclusive stage-to-digest rename, not receipt creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationPhase {
    BeforeCommit,
    AfterCommit,
    UncertainCommit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PublicationFailure {
    reason: &'static str,
    phase: PublicationPhase,
    directories: Vec<String>,
    receipts: Vec<String>,
}

impl PublicationFailure {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
    pub fn phase(&self) -> PublicationPhase {
        self.phase
    }
    /// Relative diagnostic candidates only; never ownership or cleanup authority.
    pub fn directory_residues(&self) -> &[String] {
        &self.directories
    }
    pub fn receipt_residues(&self) -> &[String] {
        &self.receipts
    }
}

/// Retains final directory/artifact/receipt descriptors and borrows the original lock.
/// Publication does not select, activate, or permanently accept this release.
pub struct PublishedRelease<'lock> {
    staged: StagedRelease<'lock>,
    #[cfg(target_os = "macos")]
    _receipt: macos::Receipt,
    receipt_name: String,
}

impl PublishedRelease<'_> {
    pub fn directory_name(&self) -> &str {
        self.staged.directory_name()
    }
    pub fn receipt_name(&self) -> &str {
        &self.receipt_name
    }

    /// Recheck the original held pair and trust inputs without accepting replacements.
    /// Success is a bounded observation, not selection or activation authority.
    pub fn revalidate(&mut self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            macos::revalidate(
                self,
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("published release verification requires macOS")
        }
    }
}

/// Open an existing digest directory and sibling receipt under the lock's layout.
/// Obtain both pins independently; receipt contents are never a trust source.
/// Names derive only from the validated supplied bytes. Nothing is created,
/// repaired, selected, activated, or inferred from `current`.
pub fn reopen_published<'lock>(
    owner: &'lock InstallerLock,
    canonical_manifest: &[u8],
    expected_digest: &[u8; 32],
    certificate_sha1: &str,
) -> Result<PublishedRelease<'lock>, &'static str> {
    #[cfg(target_os = "macos")]
    {
        macos::reopen_published(
            owner,
            canonical_manifest,
            expected_digest,
            certificate_sha1,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, canonical_manifest, expected_digest, certificate_sha1);
        Err("published release verification requires macOS")
    }
}

/// Publish only the consumed stage's retained manifest, trust, files, and lock.
/// No new path, digest, signer, or layout can be supplied. Failures never clean up.
pub fn publish(staged: StagedRelease<'_>) -> Result<PublishedRelease<'_>, PublicationFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::publish(
            staged,
            #[cfg(test)]
            &mut macos::Hooks::production(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(PublicationFailure {
            reason: "release publication requires macOS",
            phase: PublicationPhase::BeforeCommit,
            directories: vec![staged.directory_name().into()],
            receipts: Vec::new(),
        })
    }
}
