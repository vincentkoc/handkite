//! Bounded intent syntax and read-only present-state reconciliation, never authority.

use crate::current_selection::{encode_target, parse_target};
use crate::install_plan::{ManifestDigest, Operation};
use crate::installer_lock::InstallerLock;
use crate::release_stage::PublishedRelease;
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
pub(crate) mod macos;

pub const MAX_INTENT_BYTES: usize = 1024;
const INVALID: &str = "invalid selection intent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkClaim {
    manifest: ManifestDigest,
    device: i32,
    inode: u64,
}

/// Parsed claims only. Neither parsing nor encoding proves execution or evidence.
/// A future writer must generate a unique transaction ID; this codec checks syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    operation: Operation,
    transaction: [u8; 16],
    previous: Option<LinkClaim>,
    destination: LinkClaim,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WireOperation {
    InitialInstall,
    Upgrade,
    Rollback,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLink {
    manifest: String,
    device: i32,
    inode: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIntent {
    version: u32,
    transaction: String,
    operation: WireOperation,
    previous: Option<WireLink>,
    destination: WireLink,
}

impl WireLink {
    fn claim(self) -> Result<LinkClaim, &'static str> {
        if self.inode == 0 {
            return Err(INVALID);
        }
        Ok(LinkClaim {
            manifest: parse_target(self.manifest.as_bytes()).map_err(|_| INVALID)?,
            device: self.device,
            inode: self.inode,
        })
    }
}

impl From<LinkClaim> for WireLink {
    fn from(value: LinkClaim) -> Self {
        Self {
            manifest: String::from_utf8(encode_target(&value.manifest).to_vec()).unwrap(),
            device: value.device,
            inode: value.inode,
        }
    }
}

impl Intent {
    /// Accept only the exact bounded v1 canonical encoding, with no normalization.
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.is_empty() || bytes.len() > MAX_INTENT_BYTES {
            return Err(INVALID);
        }
        let wire: WireIntent = serde_json::from_slice(bytes).map_err(|_| INVALID)?;
        if wire.version != 1 || wire.transaction.len() != 32 {
            return Err(INVALID);
        }
        let mut transaction = [0; 16];
        for (pair, byte) in wire
            .transaction
            .as_bytes()
            .chunks_exact(2)
            .zip(&mut transaction)
        {
            let nibble = |byte| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(INVALID),
            };
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        if transaction == [0; 16] {
            return Err(INVALID);
        }
        let operation = match wire.operation {
            WireOperation::InitialInstall => Operation::InitialInstall,
            WireOperation::Upgrade => Operation::Upgrade,
            WireOperation::Rollback => Operation::Rollback,
        };
        let previous = wire.previous.map(WireLink::claim).transpose()?;
        let destination = wire.destination.claim()?;
        match (operation, previous) {
            (Operation::InitialInstall, None) => {}
            (Operation::Upgrade | Operation::Rollback, Some(previous))
                if previous.manifest != destination.manifest
                    && previous.device == destination.device
                    && previous.inode != destination.inode => {}
            _ => return Err(INVALID),
        }
        let value = Self {
            operation,
            transaction,
            previous,
            destination,
        };
        if value.canonical_bytes()? != bytes {
            return Err(INVALID);
        }
        Ok(value)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, &'static str> {
        let wire = WireIntent {
            version: 1,
            transaction: self
                .transaction
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            operation: match self.operation {
                Operation::InitialInstall => WireOperation::InitialInstall,
                Operation::Upgrade => WireOperation::Upgrade,
                Operation::Rollback => WireOperation::Rollback,
            },
            previous: self.previous.map(Into::into),
            destination: self.destination.into(),
        };
        serde_json::to_vec(&wire).map_err(|_| INVALID)
    }
}

/// Present matching evidence only, never proof of a historical commit or stopped job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    PreviousSelection,
    DestinationSelection,
    IndeterminateConflict,
}

/// Holds the actual selection snapshot, verified release borrows, and installer lock.
/// There is no public constructor or conversion to installation/activation authority.
pub struct Inspection<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::Retained<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported:
        std::marker::PhantomData<(&'held mut PublishedRelease<'lock>, &'lock InstallerLock)>,
}

impl Inspection<'_, '_> {
    /// Reverify the retained releases and unchanged full current-link snapshot.
    /// A changed observation fails; it never replaces the original baseline.
    pub fn reobserve(&mut self) -> Result<Observation, &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.reobserve(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err("selection recovery inspection requires macOS")
        }
    }
}

/// Reconcile intent claims against an anchored current snapshot and actual releases.
/// Required handles must retain this exact lock and match independently pinned
/// manifests. No intent record or caller-supplied enum can substitute for them.
pub fn inspect<'held, 'lock>(
    owner: &'lock InstallerLock,
    intent: &Intent,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
) -> Result<(Inspection<'held, 'lock>, Observation), &'static str> {
    #[cfg(target_os = "macos")]
    {
        macos::inspect(
            owner,
            intent,
            previous,
            destination,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, intent, previous, destination);
        Err("selection recovery inspection requires macOS")
    }
}

#[cfg(test)]
mod tests;
