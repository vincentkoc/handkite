use super::StagedRelease;

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
