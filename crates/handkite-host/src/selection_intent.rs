//! Bounded intent syntax, retained selection transactions, and reconciliation.
//!
//! Parsing and read-only inspection remain public:
//!
//! ```
//! use handkite_host::selection_intent::{Intent, inspect};
//! ```
//!
//! Initial selection and explicit updates have public coordinators. Individual transaction
//! capabilities remain private and consume independently pinned device agreement.
//!
//! ```compile_fail,E0603
//! use handkite_host::selection_intent::prepare;
//! ```
//!
//! ```compile_fail,E0603
//! use handkite_host::selection_intent::commit;
//! ```
//!
//! ```compile_fail,E0603
//! use handkite_host::selection_intent::retire;
//! ```
//!
//! ```compile_fail,E0603
//! use handkite_host::selection_intent::RetiredSelection;
//! ```
//!
//! Retained pending-journal inspection remains an internal recovery prerequisite:
//!
//! ```compile_fail,E0603
//! use handkite_host::selection_intent::inspect_pending;
//! ```

use crate::current_selection::{encode_target, parse_target};
use crate::device_agreement::DeviceAgreement;
use crate::install_plan::{ManifestDigest, Operation};
use crate::installer_lock::InstallerLock;
use crate::release_stage::PublishedRelease;
use serde::{Deserialize, Serialize};

mod update;
pub use update::{
    SelectionFailure, SelectionPhase, SelectionStage, UpdateOperation, select_update,
};

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
/// The preparation writer generates the transaction ID; this codec checks syntax.
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

/// Fixed sibling journal name under the lock's anchored releases directory.
pub const PENDING_JOURNAL: &str = ".selection-pending.json";

/// Retains the fixed journal, current snapshot, verified releases, and exact lock.
/// Journal claims provide no historical commit, cleanup, or installation authority.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PendingInspection<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::pending::Pending<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported:
        std::marker::PhantomData<(&'held mut PublishedRelease<'lock>, &'lock InstallerLock)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PendingInspection<'_, '_> {
    /// Recheck the same journal, selection, and real signer evidence without adoption.
    pub(crate) fn reobserve(&mut self) -> Result<Observation, &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.reobserve(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        Err("pending selection inspection requires macOS")
    }
}

/// Read only the fixed pending journal relative to the anchored releases directory.
/// Missing, partial, unsafe, or changed evidence refuses without repair or mutation.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn inspect_pending<'held, 'lock>(
    owner: &'lock InstallerLock,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
) -> Result<(PendingInspection<'held, 'lock>, Observation), &'static str> {
    #[cfg(target_os = "macos")]
    {
        macos::pending::inspect_pending(
            owner,
            previous,
            destination,
            #[cfg(test)]
            None,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, previous, destination);
        Err("pending selection inspection requires macOS")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PreparationFailure {
    reason: &'static str,
    residues: Vec<String>,
}

impl PreparationFailure {
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// Relative candidates only, including uncertain creates; never cleanup authority.
    pub fn residue_candidates(&self) -> &[String] {
        &self.residues
    }
}

/// Owns the device agreement and retains lock, releases, current, link and journal.
/// Dropping it preserves every file. Only consuming `commit` can select a release.
pub struct PreparedSelection<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::preparation::Prepared<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported: std::marker::PhantomData<(
        DeviceAgreement<'lock>,
        &'held mut PublishedRelease<'lock>,
        &'lock InstallerLock,
    )>,
}

impl PreparedSelection<'_, '_> {
    /// Recheck config, custody and real signer policy without device transport.
    /// This does not refresh device presence or adopt changed evidence.
    pub fn revalidate(&mut self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        Err("selection preparation requires macOS")
    }
}

/// Create one unique sibling link and one exclusive fixed canonical intent journal.
/// Current must match the real previous release, or be absent for initial install.
/// The consumed device agreement and release handles must retain this exact lock.
/// Reobserve the original device after signer checks and before the first write.
/// All errors preserve residues, with no retry, rollback, adoption, or cleanup.
// Only guarded selection coordinators are exposed to production callers.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn prepare<'held, 'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    operation: Operation,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
) -> Result<PreparedSelection<'held, 'lock>, PreparationFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::preparation::prepare(
            owner,
            agreement,
            operation,
            previous,
            destination,
            #[cfg(test)]
            &mut macos::preparation::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, agreement, operation, previous, destination);
        Err(PreparationFailure {
            reason: "selection preparation requires macOS",
            residues: Vec::new(),
        })
    }
}

/// The commit point is the single candidate-to-current exclusive rename or swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitPhase {
    BeforeCommit,
    AfterCommit,
    UncertainCommit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommitFailure {
    reason: &'static str,
    phase: CommitPhase,
    residues: Vec<String>,
}

impl CommitFailure {
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn phase(&self) -> CommitPhase {
        self.phase
    }

    /// Relative diagnostic candidates only, never ownership or cleanup authority.
    pub fn residue_candidates(&self) -> &[String] {
        &self.residues
    }
}

/// Owns the same device agreement with selected and exchanged links, releases and lock.
/// This is bounded selection evidence, never activation or power-loss recovery proof.
pub struct CommittedSelection<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::preparation::commit::Committed<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported: std::marker::PhantomData<(
        DeviceAgreement<'lock>,
        &'held mut PublishedRelease<'lock>,
        &'lock InstallerLock,
    )>,
}

impl CommittedSelection<'_, '_> {
    /// Check committed names, config, custody and signers without device transport.
    /// This provides no post-commit device observation or replacement baseline.
    pub fn revalidate(&mut self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        Err("selection commit requires macOS")
    }
}

/// Consume the prepared capability with its original device agreement and custody.
/// Reobserve before commit inventory capture; no await follows any mutation.
/// Attempt at most one rename. Every failure consumes preparation and preserves files.
/// No retry, rollback, journal removal, activation, or replacement evidence is accepted.
// Public coordinators preserve the private transaction's stopped-job admission.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn commit<'held, 'lock>(
    prepared: PreparedSelection<'held, 'lock>,
) -> Result<CommittedSelection<'held, 'lock>, CommitFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::preparation::commit::commit(
            prepared,
            #[cfg(test)]
            &mut macos::preparation::commit::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = prepared;
        Err(CommitFailure {
            reason: "selection commit requires macOS",
            phase: CommitPhase::BeforeCommit,
            residues: Vec::new(),
        })
    }
}

/// The boundary is the single exclusive pending-to-completed journal rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
pub(crate) enum RetirementPhase {
    Before,
    Applied,
    Uncertain,
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RetirementFailure {
    reason: &'static str,
    phase: RetirementPhase,
    residues: Vec<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RetirementFailure {
    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }
    pub(crate) fn phase(&self) -> RetirementPhase {
        self.phase
    }
    /// Relative candidates only, including uncertain names; never cleanup authority.
    pub(crate) fn residue_candidates(&self) -> &[String] {
        &self.residues
    }
}

/// Retains the same lock, device agreement, releases, links and moved journal.
/// No conversion back to commit authority or historical recovery proof exists.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RetiredSelection<'held, 'lock> {
    #[cfg(target_os = "macos")]
    inner: macos::preparation::commit::Committed<'held, 'lock>,
    #[cfg(not(target_os = "macos"))]
    _unsupported: std::marker::PhantomData<(
        DeviceAgreement<'lock>,
        &'held mut PublishedRelease<'lock>,
        &'lock InstallerLock,
    )>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RetiredSelection<'_, '_> {
    pub(crate) fn revalidate(&mut self) -> Result<(), &'static str> {
        #[cfg(target_os = "macos")]
        {
            self.inner.revalidate(
                #[cfg(test)]
                None,
            )
        }
        #[cfg(not(target_os = "macos"))]
        Err("selection journal retirement requires macOS")
    }
}

/// Consume only a successful in-process commit, reobserve its original device,
/// then rename its journal exactly once. No await follows the first mutation.
/// Every failure preserves all resulting names without retry, repair or deletion.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn retire<'held, 'lock>(
    committed: CommittedSelection<'held, 'lock>,
) -> Result<RetiredSelection<'held, 'lock>, RetirementFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::preparation::commit::retirement::retire(
            committed,
            #[cfg(test)]
            &mut macos::preparation::commit::retirement::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = committed;
        Err(RetirementFailure {
            reason: "selection journal retirement requires macOS",
            phase: RetirementPhase::Before,
            residues: Vec::new(),
        })
    }
}

/// Failure location for one initial selection; never an instruction to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialSelectionStage {
    Preparation,
    Commit,
    Retirement,
    Verification,
}

/// Fixed diagnostics only. Every failure preserves all remaining files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialSelectionFailure {
    stage: InitialSelectionStage,
    selection_may_be_applied: bool,
    has_residue: bool,
}

impl InitialSelectionFailure {
    pub fn stage(&self) -> InitialSelectionStage {
        self.stage
    }

    /// True for any uncertain commit or failure after the commit point.
    pub fn selection_may_be_applied(&self) -> bool {
        self.selection_may_be_applied
    }

    pub fn has_residue(&self) -> bool {
        self.has_residue
    }
}

/// Select only the first published release without starting launchd or sending UI input.
///
/// The agreement and release must borrow this exact installer lock. Current must
/// be absent. Each phase reobserves the original USB/Lockdown route and requires
/// fixed user-domain job absence under the cooperative lock. This neither excludes
/// unrelated controllers nor grants activation authority. Success returns no
/// reusable transaction or service-start capability.
pub async fn select_initial<'lock>(
    owner: &'lock InstallerLock,
    agreement: DeviceAgreement<'lock>,
    destination: &mut PublishedRelease<'lock>,
) -> Result<(), InitialSelectionFailure> {
    #[cfg(target_os = "macos")]
    {
        macos::initial::select(
            owner,
            agreement,
            destination,
            #[cfg(test)]
            &mut macos::initial::Hooks::production(),
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (owner, agreement, destination);
        Err(InitialSelectionFailure {
            stage: InitialSelectionStage::Preparation,
            selection_may_be_applied: false,
            has_residue: false,
        })
    }
}

#[cfg(test)]
mod tests;
