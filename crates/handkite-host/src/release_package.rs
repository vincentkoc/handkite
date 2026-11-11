//! Explicit signing of copied build candidates; generated inventory is not trust.

use crate::release_manifest::{
    ArtifactName, JsonObject, MAX_MANIFEST_BYTES, ReleaseArtifact, ReleaseManifest, RustIdentity,
    SdkIdentity, TARGET,
};
use crate::release_signer::SignerPolicy;
use serde::Deserialize;

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
    /// Pre-signing candidate records; these hashes must never be used as final release hashes.
    pub fn candidates(&self) -> &ReleaseManifest {
        &self.candidates
    }

    pub fn certificate_sha1(&self) -> &str {
        &self.certificate
    }
}

#[cfg(test)]
mod tests;
