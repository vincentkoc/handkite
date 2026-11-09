//! Read-only checks of an existing, independently pinned directory/receipt pair.

use super::*;
use crate::installer_lock::InstallerLock;
use crate::release_files::AnchoredRelease;
use crate::release_verify::macos::ValidatedTrust;

pub(in crate::release_stage::publication) fn reopen_published<'lock>(
    owner: &'lock InstallerLock,
    canonical_manifest: &[u8],
    expected_digest: &[u8; 32],
    certificate_sha1: &str,
    #[cfg(test)] check: Option<Check<'_>>,
) -> Result<PublishedRelease<'lock>, &'static str> {
    // Trust validation precedes even lock/path observation. The receipt is data,
    // never the source of a manifest, digest, or certificate pin.
    let trust = ValidatedTrust::new(canonical_manifest, expected_digest, certificate_sha1)?;
    let directory_name =
        String::from_utf8(encode_target(expected_digest).to_vec()).map_err(|_| UNSAFE)?;
    let receipt_name = format!("{directory_name}.manifest.json");
    // The lock retains root-to-runtime anchors, including the account-owned home
    // and fixed layout root. The release anchor adds the private releases/digest dirs.
    owner.revalidate()?;
    let destination =
        AnchoredRelease::open(&owner.layout().releases_directory().join(&directory_name))?;
    let receipt = Receipt::open_existing(
        destination.publication_parent(),
        CString::new(receipt_name.as_str()).map_err(|_| UNSAFE)?,
        canonical_manifest,
    )?;
    let mut value = PublishedRelease {
        staged: StagedRelease {
            _owner: owner,
            _destination: destination,
            _canonical_manifest: canonical_manifest.into(),
            _trust: trust,
            directory_name,
        },
        _receipt: receipt,
        receipt_name,
    };
    revalidate(
        &mut value,
        #[cfg(test)]
        check,
    )?;
    Ok(value)
}

pub(in crate::release_stage::publication) fn check_pair(
    value: &PublishedRelease<'_>,
) -> Result<(), &'static str> {
    let staged = &value.staged;
    staged._owner.revalidate()?;
    staged._destination.revalidate()?;
    value._receipt.check(
        staged._destination.publication_parent(),
        &staged._canonical_manifest,
    )?;
    staged._destination.revalidate()?;
    staged._owner.revalidate()
}

pub(in crate::release_stage::publication) fn revalidate(
    value: &mut PublishedRelease<'_>,
    #[cfg(test)] check: Option<Check<'_>>,
) -> Result<(), &'static str> {
    check_pair(value)?;
    // This includes all-three hashing after the final exact signer check. Neither
    // it nor the pair checks changes any original metadata fingerprint.
    verify_held(
        &mut value.staged._destination,
        &value.staged._trust,
        #[cfg(test)]
        check,
    )?;
    check_pair(value)
}
