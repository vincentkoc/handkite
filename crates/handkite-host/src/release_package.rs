//! Explicit signing of copied build candidates; generated inventory is not trust.

use crate::release_manifest::{
    ArtifactName, JsonObject, MAX_MANIFEST_BYTES, ReleaseArtifact, ReleaseManifest, RustIdentity,
    SdkIdentity, TARGET,
};
use crate::release_signer::SignerPolicy;
use serde::Deserialize;
use std::path::Path;

#[cfg(target_os = "macos")]
mod macos;

pub const MAX_REQUEST_BYTES: usize = MAX_MANIFEST_BYTES;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    name: ArtifactName,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestJson {
    source_sha: String,
    target: String,
    rust: JsonObject<RustIdentity>,
    sdk: JsonObject<SdkIdentity>,
    candidates: [JsonObject<Candidate>; 3],
}

/// Explicit build metadata and independent pre-signing hashes. Parsing checks
/// syntax only; the operator is responsible for their provenance.
pub struct PackageRequest {
    candidates: ReleaseManifest,
    certificate: String,
}

impl PackageRequest {
    /// `certificate_sha1` is one exact trusted certificate pin, never an identity
    /// name, default signer, ad-hoc identity, or a value discovered by this tool.
    pub fn parse(bytes: &[u8], certificate_sha1: &str) -> Result<Self, &'static str> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("package request exceeds limit");
        }
        SignerPolicy::new(certificate_sha1, ArtifactName::Daemon)?;
        let JsonObject(raw): JsonObject<RequestJson> =
            serde_json::from_slice(bytes).map_err(|_| "invalid package request")?;
        if raw.target != TARGET {
            return Err("unsupported package target");
        }
        let candidates = ReleaseManifest::new(
            raw.source_sha,
            raw.rust.0,
            raw.sdk.0,
            raw.candidates.map(|JsonObject(candidate)| {
                ReleaseArtifact::new(candidate.name, candidate.size, candidate.sha256)
            }),
        )?;
        Ok(Self {
            candidates,
            certificate: certificate_sha1.into(),
        })
    }
}

impl PackageRequest {
    pub fn source_sha(&self) -> &str {
        self.candidates.source_sha()
    }

    pub fn certificate_sha1(&self) -> &str {
        &self.certificate
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackagePhase {
    Admission,
    Copy,
    Signing,
    Inventory,
    Verification,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PackageFailure {
    phase: PackagePhase,
    residue: bool,
}

impl PackageFailure {
    pub fn phase(&self) -> PackagePhase {
        self.phase
    }

    /// True after this invocation creates its output root. Failure never removes
    /// or retries any output, including a possibly partially signed artifact.
    pub fn residue_present(&self) -> bool {
        self.residue
    }
}

/// Candidate inventory for independent review, never an acceptance capability.
pub struct PackagedRelease {
    manifest_sha256: [u8; 32],
}

impl PackagedRelease {
    pub fn manifest_sha256(&self) -> &[u8; 32] {
        &self.manifest_sha256
    }
}

/// Sign exactly three copies in a new private output root and write canonical
/// `manifest.json` beside `artifacts`. Source files are never modified. Success
/// checks final signed bytes using the existing verifier, with the generated
/// digest used only for internal consistency, not independently approved trust.
/// Same-account races between observations remain possible. No installation,
/// selection, activation, notarization, or keychain administration is performed.
pub fn package(
    source: &Path,
    output: &Path,
    request: &PackageRequest,
) -> Result<PackagedRelease, PackageFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::package(
            source,
            output,
            request,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source, output, &request.candidates, &request.certificate);
        Err(PackageFailure {
            phase: PackagePhase::Admission,
            residue: false,
        })
    }
}

#[cfg(test)]
mod tests;
