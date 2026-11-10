//! Read-only, stage-only checks against independently supplied trust inputs.

use std::path::Path;

/// Require an empty ACL on a held input file, failing closed outside macOS.
///
/// This checks only ACL metadata; callers must separately validate the file's
/// type, owner, mode, link count, and size. It does not establish trust in bytes.
pub fn check_private_input_acl(input: &std::fs::File) -> Result<(), &'static str> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        crate::release_files::check_acl(input.as_raw_fd(), true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = input;
        Err("private input ACL verification requires macOS")
    }
}

/// Check an existing private stage against the exact supplied canonical bytes.
///
/// Obtain `expected_manifest_sha256` and the exact signer certificate SHA1 from
/// independently trusted sources, never from the candidate directory. Success
/// describes these observations only: same-account replacement/restoration
/// between checks (ABA) remains possible. It is not permanent release acceptance,
/// installation, notarization, Gatekeeper, revocation, or entitlement proof.
/// Nothing is created, signed, installed, activated, or executed from the stage.
pub fn verify(
    stage: &Path,
    manifest_bytes: &[u8],
    expected_manifest_sha256: &[u8; 32],
    certificate_sha1: &str,
) -> Result<(), &'static str> {
    #[cfg(target_os = "macos")]
    {
        macos::verify(
            stage,
            manifest_bytes,
            expected_manifest_sha256,
            certificate_sha1,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            stage,
            manifest_bytes,
            expected_manifest_sha256,
            certificate_sha1,
        );
        Err("release file verification requires macOS")
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use super::*;
    use crate::release_files::{AnchoredRelease, ArtifactFile, NAMES};
    use crate::release_manifest::{ReleaseArtifact, ReleaseManifest};
    use crate::release_signer::SignerPolicy;
    use std::io::{Read, Seek, SeekFrom};

    #[cfg(test)]
    type TestCheck<'a> =
        &'a mut dyn FnMut(crate::release_manifest::ArtifactName, &Path) -> Result<(), &'static str>;

    /// Bounded caller trust inputs, not evidence about any release files.
    pub(crate) struct ValidatedTrust {
        manifest: ReleaseManifest,
        policies: [SignerPolicy; 3],
    }

    impl ValidatedTrust {
        pub(crate) fn new(
            manifest_bytes: &[u8],
            expected_manifest_sha256: &[u8; 32],
            certificate_sha1: &str,
        ) -> Result<Self, &'static str> {
            // Parse and validate both independent trust inputs before any path access.
            let manifest = ReleaseManifest::parse(manifest_bytes)?;
            if ring::digest::digest(&ring::digest::SHA256, manifest_bytes).as_ref()
                != expected_manifest_sha256
            {
                return Err("release manifest pin mismatch");
            }
            if manifest.canonical_json()? != manifest_bytes {
                return Err("release manifest is not canonical");
            }
            let policies = NAMES.map(|name| SignerPolicy::new(certificate_sha1, name));
            let [daemon, cli, mcp] = policies;
            Ok(Self {
                manifest,
                policies: [daemon?, cli?, mcp?],
            })
        }

        pub(crate) fn manifest(&self) -> &ReleaseManifest {
            &self.manifest
        }
    }

    pub(super) fn verify(
        stage: &Path,
        manifest_bytes: &[u8],
        expected_manifest_sha256: &[u8; 32],
        certificate_sha1: &str,
        #[cfg(test)] check: Option<TestCheck<'_>>,
    ) -> Result<(), &'static str> {
        let trust =
            ValidatedTrust::new(manifest_bytes, expected_manifest_sha256, certificate_sha1)?;
        let mut held = AnchoredRelease::open(stage)?;
        verify_held(
            &mut held,
            &trust,
            #[cfg(test)]
            check,
        )
    }

    /// Check the already-held files under the same fixed policy as the public route.
    /// Success remains a bounded observation, never an acceptance token.
    pub(crate) fn verify_held(
        held: &mut AnchoredRelease,
        trust: &ValidatedTrust,
        #[cfg(test)] mut check: Option<TestCheck<'_>>,
    ) -> Result<(), &'static str> {
        hash_all(held, trust.manifest())?;
        held.revalidate()?;
        for (name, policy) in NAMES.into_iter().zip(&trust.policies) {
            held.revalidate()?;
            let path = held.path(name);
            #[cfg(test)]
            if let Some(check) = check.as_mut() {
                check(name, &path)?;
            } else {
                policy.check(&path)?;
            }
            #[cfg(not(test))]
            policy.check(&path)?;
            held.revalidate()?;
        }
        // An early binary may change during a later signature check. Rehash ALL
        // three held descriptors after the final tool call, then check paths again.
        rehash_held(held, trust)
    }

    pub(crate) fn rehash_held(
        held: &mut AnchoredRelease,
        trust: &ValidatedTrust,
    ) -> Result<(), &'static str> {
        hash_all(held, trust.manifest())?;
        held.revalidate()
    }

    fn hash_all(
        held: &mut AnchoredRelease,
        manifest: &ReleaseManifest,
    ) -> Result<(), &'static str> {
        for (name, file) in NAMES.into_iter().zip(&mut held.artifacts) {
            let artifact = manifest
                .artifacts()
                .iter()
                .find(|item| item.name == name)
                .ok_or("release artifact is missing")?;
            hash_file(file, artifact)?;
        }
        Ok(())
    }

    fn hash_file(file: &mut ArtifactFile, artifact: &ReleaseArtifact) -> Result<(), &'static str> {
        const INVALID: &str = "release artifact bytes mismatch";
        if file.fingerprint.size != artifact.size {
            return Err(INVALID);
        }
        file.file.seek(SeekFrom::Start(0)).map_err(|_| INVALID)?;
        // The parsed manifest caps size at 1 GiB. Read at most that size plus one
        // byte, using fixed memory, even if a same-account writer grows the file.
        let mut reader = (&mut file.file).take(artifact.size + 1);
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 64 * 1024];
        let mut size = 0;
        loop {
            let read = reader.read(&mut buffer).map_err(|_| INVALID)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            digest.update(&buffer[..read]);
        }
        let actual: String = digest
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if size != artifact.size || actual != artifact.sha256 {
            return Err(INVALID);
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests;
}
