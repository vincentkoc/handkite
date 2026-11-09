//! Pure release inventory encoding and parsing, without artifact or signer proof.

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const TARGET: &str = "aarch64-apple-darwin";
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum ArtifactName {
    Daemon,
    Cli,
    Mcp,
}

impl TryFrom<String> for ArtifactName {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "handkited" => Ok(Self::Daemon),
            "handkitectl" => Ok(Self::Cli),
            "handkite-mcp" => Ok(Self::Mcp),
            _ => Err("unsupported release artifact name"),
        }
    }
}

impl ArtifactName {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Daemon => "handkited",
            Self::Cli => "handkitectl",
            Self::Mcp => "handkite-mcp",
        }
    }

    pub fn code_identifier(self) -> &'static str {
        match self {
            Self::Daemon => "com.handkite.handkited",
            Self::Cli => "com.handkite.handkitectl",
            Self::Mcp => "com.handkite.handkite-mcp",
        }
    }
}

impl Serialize for ArtifactName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.filename())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub name: ArtifactName,
    pub size: u64,
    /// Lowercase SHA-256 of the final signed executable bytes.
    pub sha256: String,
    pub code_identifier: String,
}

impl ReleaseArtifact {
    /// Record caller-supplied final signed-byte metadata. The manifest constructor
    /// validates this record; no executable bytes or signatures are checked here.
    pub fn new(name: ArtifactName, size: u64, sha256: String) -> Self {
        Self {
            name,
            size,
            sha256,
            code_identifier: name.code_identifier().into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RustIdentity {
    /// Exact stable toolchain version, without a host suffix or channel alias.
    pub toolchain: String,
    pub compiler_commit: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SdkIdentity {
    pub version: String,
    pub build: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestJson {
    schema_version: u32,
    source_sha: String,
    target: String,
    rust: JsonObject<RustIdentity>,
    sdk: JsonObject<SdkIdentity>,
    artifacts: [JsonObject<ReleaseArtifact>; 3],
}

/// Structurally validated inventory, not release acceptance or signature proof.
#[derive(Debug)]
pub struct ReleaseManifest {
    source_sha: String,
    rust: RustIdentity,
    sdk: SdkIdentity,
    artifacts: [ReleaseArtifact; 3],
}

impl ReleaseManifest {
    /// Parse supplied bytes only. A caller must externally pin this manifest's
    /// digest and independently verify artifacts against a separate signer policy.
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("release manifest exceeds 16 KiB");
        }
        let JsonObject(raw): JsonObject<ManifestJson> =
            serde_json::from_slice(bytes).map_err(|_| "invalid release manifest JSON")?;
        if raw.schema_version != SCHEMA_VERSION || raw.target != TARGET {
            return Err("unsupported release schema or target");
        }
        Self::new(
            raw.source_sha,
            raw.rust.0,
            raw.sdk.0,
            raw.artifacts.map(|item| item.0),
        )
    }

    /// Construct a v1 inventory using the same validation as the JSON parser.
    /// The caller supplies build identities and final signed-byte metadata.
    pub fn new(
        source_sha: String,
        rust: RustIdentity,
        sdk: SdkIdentity,
        artifacts: [ReleaseArtifact; 3],
    ) -> Result<Self, &'static str> {
        let value = Self {
            source_sha,
            rust,
            sdk,
            artifacts,
        };
        if !lower_hex(&value.source_sha, 40)
            || !version(&value.rust.toolchain, 3, 3)
            || !lower_hex(&value.rust.compiler_commit, 40)
            || !version(&value.sdk.version, 2, 3)
            || value.sdk.build.is_empty()
            || value.sdk.build.len() > 32
            || !value
                .sdk
                .build
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err("invalid release build identity");
        }
        for name in [ArtifactName::Daemon, ArtifactName::Cli, ArtifactName::Mcp] {
            if value
                .artifacts
                .iter()
                .filter(|item| item.name == name)
                .count()
                != 1
            {
                return Err("release must contain each application exactly once");
            }
        }
        for artifact in &value.artifacts {
            if !(1..=MAX_ARTIFACT_BYTES).contains(&artifact.size)
                || !lower_hex(&artifact.sha256, 64)
                || artifact.code_identifier != artifact.name.code_identifier()
            {
                return Err("invalid release artifact");
            }
        }
        Ok(value)
    }

    /// Compact UTF-8 JSON with fixed field order, no trailing newline, and
    /// artifacts ordered as daemon, CLI, MCP. This is Handkite v1's encoding.
    pub fn canonical_json(&self) -> Result<Vec<u8>, &'static str> {
        #[derive(Serialize)]
        struct Output<'a> {
            schema_version: u32,
            source_sha: &'a str,
            target: &'a str,
            rust: &'a RustIdentity,
            sdk: &'a SdkIdentity,
            artifacts: [&'a ReleaseArtifact; 3],
        }

        let mut artifacts = self.artifacts.each_ref();
        artifacts.sort_unstable_by_key(|artifact| match artifact.name {
            ArtifactName::Daemon => 0,
            ArtifactName::Cli => 1,
            ArtifactName::Mcp => 2,
        });
        serde_json::to_vec(&Output {
            schema_version: SCHEMA_VERSION,
            source_sha: &self.source_sha,
            target: TARGET,
            rust: &self.rust,
            sdk: &self.sdk,
            artifacts,
        })
        .map_err(|_| "cannot encode release manifest")
    }

    /// SHA-256 of `canonical_json()`, for an independently pinned receipt.
    /// This is not the digest of the original bytes supplied to `parse`.
    pub fn canonical_sha256(&self) -> Result<[u8; 32], &'static str> {
        let digest = ring::digest::digest(&ring::digest::SHA256, &self.canonical_json()?);
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Ok(bytes)
    }

    pub fn source_sha(&self) -> &str {
        &self.source_sha
    }

    pub fn rust(&self) -> &RustIdentity {
        &self.rust
    }

    pub fn sdk(&self) -> &SdkIdentity {
        &self.sdk
    }

    pub fn artifacts(&self) -> &[ReleaseArtifact; 3] {
        &self.artifacts
    }
}

// Derived struct deserializers also accept JSON arrays. Require objects before
// delegating field checks, preserving duplicate-field detection during parsing.
#[derive(Debug)]
struct JsonObject<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for JsonObject<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = JsonObject<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(JsonObject)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
    }
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn version(value: &str, min_parts: usize, max_parts: usize) -> bool {
    let parts = value.split('.');
    (min_parts..=max_parts).contains(&parts.clone().count())
        && parts.clone().all(|part| {
            (1..=3).contains(&part.len())
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        })
}

#[cfg(test)]
mod tests;
