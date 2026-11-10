use super::*;
use crate::current_selection::macos::RetainedLink;
use crate::release_files::Fingerprint;
use ring::rand::{SecureRandom, SystemRandom};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

pub(crate) mod commit;
mod journal;
mod parent_change;
use journal::Journal;
use parent_change::BeforeChange;
pub(crate) use parent_change::ParentChange;

const UNSAFE: &str = "unsafe or changed selection preparation";
const JOURNAL: &CStr = c".selection-pending.json";

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    BeforeLink,
    LinkCreated,
    LinkSynced,
    BeforeJournal,
    JournalCreated,
    JournalWritten,
    JournalReadonly,
    JournalSynced,
    BeforeFinalVerify,
    Complete,
}

#[cfg(test)]
type Fault<'a> = &'a mut dyn FnMut(Point) -> Result<(), &'static str>;

#[cfg(test)]
pub(crate) struct Hooks<'a> {
    pub(crate) check: Option<Check<'a>>,
    pub(crate) fault: Option<Fault<'a>>,
    pub(crate) nonce: Option<[u8; 16]>,
}

#[cfg(test)]
impl Hooks<'_> {
    pub(crate) fn production() -> Self {
        Self {
            check: None,
            fault: None,
            nonce: None,
        }
    }
    fn at(&mut self, point: Point) -> Result<(), &'static str> {
        if let Some(fault) = self.fault.as_mut() {
            fault(point)?;
        }
        Ok(())
    }
}

pub(in crate::selection_intent) struct Prepared<'held, 'lock> {
    config: &'held mut RetainedConfig<'lock>,
    retained: Retained<'held, 'lock>,
    link: RetainedLink,
    previous_link: Option<RetainedLink>,
    journal: Journal,
}

impl Prepared<'_, '_> {
    fn custody(&self) -> Result<(), &'static str> {
        self.link
            .check(self.retained.destination.selection_parent())?;
        if let Some(previous) = &self.previous_link {
            previous.check(self.retained.destination.selection_parent())?;
        }
        self.journal
            .check(self.retained.destination.selection_parent())
    }

    pub(crate) fn revalidate(
        &mut self,
        #[cfg(test)] check: Option<Check<'_>>,
    ) -> Result<(), &'static str> {
        self.config.revalidate_for_owner(self.retained.owner)?;
        self.custody()?;
        if self.retained.reobserve(
            #[cfg(test)]
            check,
        )? != Observation::PreviousSelection
        {
            return Err(UNSAFE);
        }
        self.config.revalidate_for_owner(self.retained.owner)?;
        self.custody()?;
        self.retained.destination.revalidate_custody()?;
        if let Some(previous) = &self.retained.previous {
            previous.revalidate_custody()?;
        }
        self.retained.snapshot.reobserve_unchanged()?;
        self.retained.owner.revalidate()?;
        self.config.revalidate_for_owner(self.retained.owner)
    }
}

fn checkpoint(
    retained: &Retained<'_, '_>,
    config: &mut RetainedConfig<'_>,
    link: Option<&RetainedLink>,
) -> Result<(), &'static str> {
    config.revalidate_for_owner(retained.owner)?;
    retained.owner.revalidate()?;
    retained.snapshot.reobserve_unchanged()?;
    if let Some(previous) = &retained.previous {
        previous.revalidate_custody()?;
    }
    retained.destination.revalidate_custody()?;
    if let Some(link) = link {
        link.check(retained.destination.selection_parent())?;
    }
    config.revalidate_for_owner(retained.owner)
}

fn transition(
    retained: &mut Retained<'_, '_>,
    config: &mut RetainedConfig<'_>,
    change: &ParentChange,
) -> Result<(), &'static str> {
    config.revalidate_for_owner(retained.owner)?;
    retained.owner.revalidate()?;
    retained.snapshot.reobserve_unchanged()?;
    if let Some(previous) = &mut retained.previous {
        previous.selection_parent_transition(change)?;
    }
    retained.destination.selection_parent_transition(change)?;
    checkpoint(retained, config, None)
}

pub(crate) fn prepare<'held, 'lock>(
    owner: &'lock InstallerLock,
    config: &'held mut RetainedConfig<'lock>,
    operation: Operation,
    previous: Option<&'held mut PublishedRelease<'lock>>,
    destination: &'held mut PublishedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<PreparedSelection<'held, 'lock>, PreparationFailure> {
    let mut residues = Vec::new();
    let result = (|| {
        config.revalidate_for_owner(owner)?;
        owner.revalidate()?;
        let destination_digest = parse_target(destination.directory_name().as_bytes())?;
        if !destination.matches_owner_and_digest(owner, &destination_digest) {
            return Err(MISMATCH);
        }
        let snapshot = Snapshot::capture(owner.layout())?;
        let previous_link = snapshot.retain_current()?;
        let previous_claim = match (
            operation,
            &previous,
            snapshot.selected_digest(),
            snapshot.selected_link_identity(),
        ) {
            (Operation::InitialInstall, None, None, None) => None,
            (
                Operation::Upgrade | Operation::Rollback,
                Some(release),
                Some(manifest),
                Some((device, inode)),
            ) if manifest != destination_digest
                && release.matches_owner_and_digest(owner, &manifest) =>
            {
                Some(LinkClaim {
                    manifest,
                    device,
                    inode,
                })
            }
            _ => return Err(MISMATCH),
        };
        let mut transaction = [0; 16];
        SystemRandom::new()
            .fill(&mut transaction)
            .map_err(|_| "selection transaction ID unavailable")?;
        #[cfg(test)]
        if let Some(nonce) = hooks.nonce {
            transaction = nonce;
        }
        if transaction == [0; 16] {
            return Err("selection transaction ID unavailable");
        }
        let candidate_name = format!(
            ".selection-{}",
            transaction
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let candidate = CString::new(candidate_name.as_str()).map_err(|_| UNSAFE)?;
        let mut retained = Retained {
            owner,
            snapshot,
            previous,
            destination,
            // The destination inode is filled only after the actual link is retained.
            intent: Intent {
                operation,
                transaction,
                previous: previous_claim,
                destination: LinkClaim {
                    manifest: destination_digest,
                    device: 0,
                    inode: 0,
                },
            },
        };
        config.revalidate_for_owner(owner)?;
        #[cfg(test)]
        let observed = if let Some(check) = hooks.check.as_mut() {
            retained.reobserve(Some(&mut **check))?
        } else {
            retained.reobserve(None)?
        };
        #[cfg(not(test))]
        let observed = retained.reobserve()?;
        config.revalidate_for_owner(owner)?;
        if observed != Observation::PreviousSelection {
            return Err(MISMATCH);
        }
        BeforeChange::capture(retained.destination.selection_parent(), JOURNAL)?;
        let before = BeforeChange::capture(retained.destination.selection_parent(), &candidate)?;
        #[cfg(test)]
        hooks.at(Point::BeforeLink)?;
        checkpoint(&retained, config, None)?;
        residues.push(candidate_name);
        let target =
            CString::new(encode_target(&destination_digest).as_slice()).map_err(|_| UNSAFE)?;
        if unsafe {
            libc::symlinkat(
                target.as_ptr(),
                retained.destination.selection_parent().as_raw_fd(),
                candidate.as_ptr(),
            )
        } != 0
        {
            return Err("selection candidate creation failed");
        }
        #[cfg(test)]
        hooks.at(Point::LinkCreated)?;
        let link = RetainedLink::open(
            retained.destination.selection_parent(),
            candidate.clone(),
            destination_digest,
        )?;
        let change = before.finish(
            retained.destination.selection_parent(),
            &candidate,
            link.identity(),
        )?;
        transition(&mut retained, config, &change)?;
        checkpoint(&retained, config, Some(&link))?;
        retained
            .destination
            .selection_parent()
            .sync_all()
            .map_err(|_| "selection candidate sync failed")?;
        #[cfg(test)]
        hooks.at(Point::LinkSynced)?;
        checkpoint(&retained, config, Some(&link))?;
        let (device, inode) = link.identity();
        retained.intent.destination = LinkClaim {
            manifest: destination_digest,
            device,
            inode,
        };
        let bytes = retained.intent.canonical_bytes()?;
        Intent::parse(&bytes)?;
        let before = BeforeChange::capture(retained.destination.selection_parent(), JOURNAL)?;
        #[cfg(test)]
        hooks.at(Point::BeforeJournal)?;
        checkpoint(&retained, config, Some(&link))?;
        residues.push(PENDING_JOURNAL.into());
        let fd = unsafe {
            libc::openat(
                retained.destination.selection_parent().as_raw_fd(),
                JOURNAL.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err("selection journal creation failed");
        }
        let mut writer = unsafe { File::from_raw_fd(fd) };
        #[cfg(test)]
        hooks.at(Point::JournalCreated)?;
        journal::check_writer(&writer, retained.destination.selection_parent(), 0o600, b"")?;
        let metadata = writer.metadata().map_err(|_| UNSAFE)?;
        let change = before.finish(
            retained.destination.selection_parent(),
            JOURNAL,
            (metadata.dev() as i32, metadata.ino()),
        )?;
        transition(&mut retained, config, &change)?;
        checkpoint(&retained, config, Some(&link))?;
        journal::check_writer(&writer, retained.destination.selection_parent(), 0o600, b"")?;
        writer
            .write_all(&bytes)
            .map_err(|_| "selection journal write failed")?;
        #[cfg(test)]
        hooks.at(Point::JournalWritten)?;
        checkpoint(&retained, config, Some(&link))?;
        journal::check_writer(
            &writer,
            retained.destination.selection_parent(),
            0o600,
            &bytes,
        )?;
        if unsafe { libc::fchmod(writer.as_raw_fd(), 0o400) } != 0 {
            return Err("selection journal protection failed");
        }
        #[cfg(test)]
        hooks.at(Point::JournalReadonly)?;
        journal::check_writer(
            &writer,
            retained.destination.selection_parent(),
            0o400,
            &bytes,
        )?;
        checkpoint(&retained, config, Some(&link))?;
        writer
            .sync_all()
            .map_err(|_| "selection journal sync failed")?;
        let journal = Journal::retain(writer, retained.destination.selection_parent(), bytes)?;
        #[cfg(test)]
        hooks.at(Point::JournalSynced)?;
        let mut value = Prepared {
            config,
            retained,
            link,
            previous_link,
            journal,
        };
        value.custody()?;
        checkpoint(&value.retained, value.config, Some(&value.link))?;
        value
            .retained
            .destination
            .selection_parent()
            .sync_all()
            .map_err(|_| "selection journal parent sync failed")?;
        #[cfg(test)]
        hooks.at(Point::BeforeFinalVerify)?;
        #[cfg(test)]
        if let Some(check) = hooks.check.as_mut() {
            value.revalidate(Some(&mut **check))?;
        } else {
            value.revalidate(None)?;
        }
        #[cfg(not(test))]
        value.revalidate()?;
        #[cfg(test)]
        hooks.at(Point::Complete)?;
        value.custody()?;
        checkpoint(&value.retained, value.config, Some(&value.link))?;
        Ok(PreparedSelection { inner: value })
    })();
    result.map_err(|reason| PreparationFailure { reason, residues })
}

#[cfg(test)]
pub(crate) fn revalidate_for_test(
    value: &mut PreparedSelection<'_, '_>,
    check: Check<'_>,
) -> Result<(), &'static str> {
    value.inner.revalidate(Some(check))
}
