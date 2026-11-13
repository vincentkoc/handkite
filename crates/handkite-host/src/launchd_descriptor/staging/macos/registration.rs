use super::*;
use crate::current_selection::macos::Snapshot;
use crate::device_agreement::DeviceAgreement;
use crate::launchd_descriptor::{RegistrationFailure, RegistrationOutcome};

#[cfg(test)]
pub(crate) struct Hooks<'a> {
    pub(crate) files: super::Hooks<'a>,
    pub(crate) device: Option<crate::device_agreement::tests::Fake>,
}

#[cfg(test)]
impl Hooks<'_> {
    pub(crate) fn production() -> Self {
        Self {
            files: super::Hooks::production(),
            device: None,
        }
    }
}

fn guard(
    owner: &InstallerLock,
    agreement: &mut DeviceAgreement<'_>,
    selected: &Snapshot,
    staged: &mut StagedDescriptor<'_, '_>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<(), &'static str> {
    owner.revalidate()?;
    agreement.revalidate_for_owner(owner)?;
    selected.reobserve_unchanged()?;
    staged.inner.revalidate(
        #[cfg(test)]
        hooks.files.check.as_mut().map(|check| &mut **check as _),
    )?;
    owner.require_jobs_absent()?;
    selected.reobserve_unchanged()?;
    agreement.revalidate_for_owner(owner)
}

pub(crate) async fn register<'lock>(
    owner: &'lock InstallerLock,
    mut agreement: DeviceAgreement<'lock>,
    config: &mut RetainedConfig<'lock>,
    release: &mut PublishedRelease<'lock>,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<RegistrationOutcome, RegistrationFailure> {
    let mut phase = StagePhase::BeforeRename;
    let mut residue = false;
    let result = async {
        owner.require_jobs_absent()?;
        agreement.revalidate_for_owner(owner)?;
        let selected = Snapshot::capture(owner.layout())?;
        let digest = crate::current_selection::parse_target(release.directory_name().as_bytes())?;
        if selected.selected_digest() != Some(digest) {
            return Err("selected release mismatch");
        }
        let mut staged = super::reopen_staged(
            owner,
            config,
            release,
            #[cfg(test)]
            hooks.files.check.as_mut().map(|check| &mut **check as _),
        )?;
        #[cfg(test)]
        if let Some(device) = hooks.device.take() {
            crate::device_agreement::tests::reobserve(&mut agreement, device).await?;
        } else {
            agreement.reobserve().await?;
        }
        #[cfg(not(test))]
        agreement.reobserve().await?;
        // The remainder cannot suspend after any filesystem mutation.
        guard(
            owner,
            &mut agreement,
            &selected,
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        let (mut anchors, exists) = Anchors::open_registration(owner.layout())?;
        let bytes = staged.inner.descriptor.bytes.clone();
        if exists {
            let descriptor = Descriptor::open(&anchors, bytes)?;
            guard(
                owner,
                &mut agreement,
                &selected,
                &mut staged,
                #[cfg(test)]
                hooks,
            )?;
            descriptor.check(&anchors, FINAL)?;
            anchors.check(&[FINAL])?;
            return Ok(RegistrationOutcome::VerifiedExisting);
        }
        let temporary = name(
            #[cfg(test)]
            &hooks.files,
        )?;
        #[cfg(test)]
        hooks.files.at(Point::BeforeCreate)?;
        guard(
            owner,
            &mut agreement,
            &selected,
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        anchors.check(&[])?;
        let fd = unsafe {
            libc::openat(
                anchors.parent().as_raw_fd(),
                temporary.as_ptr(),
                libc::O_RDWR
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CREAT
                    | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err(UNSAFE);
        }
        residue = true;
        let mut writer = unsafe { File::from_raw_fd(fd) };
        file_policy(&writer, 0o600, 0)?;
        let original = writer.metadata().map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.files.at(Point::Created)?;
        anchors.transition(&[&temporary], true)?;
        writer.write_all(&bytes).map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.files.at(Point::Written)?;
        file_policy(&writer, 0o600, bytes.len())?;
        bytes_match(&writer, &bytes)?;
        if unsafe { libc::fchmod(writer.as_raw_fd(), 0o400) } != 0 {
            return Err(UNSAFE);
        }
        #[cfg(test)]
        hooks.files.at(Point::Readonly)?;
        writer.sync_all().map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.files.at(Point::Synced)?;
        let fingerprint = file_policy(&writer, 0o400, bytes.len())?;
        let after = writer.metadata().map_err(|_| UNSAFE)?;
        if (original.dev(), original.ino(), original.gid())
            != (after.dev(), after.ino(), after.gid())
        {
            return Err(UNSAFE);
        }
        let file = open_at(anchors.parent().as_raw_fd(), &temporary, false)?;
        if file_policy(&file, 0o400, bytes.len())? != fingerprint {
            return Err(UNSAFE);
        }
        bytes_match(&file, &bytes)?;
        drop(writer);
        let mut descriptor = Descriptor {
            file,
            fingerprint,
            bytes,
        };
        #[cfg(test)]
        hooks.files.at(Point::BeforeRename)?;
        guard(
            owner,
            &mut agreement,
            &selected,
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        anchors.check(&[&temporary])?;
        descriptor.check(&anchors, &temporary)?;
        #[cfg(test)]
        hooks.files.at(Point::BeforeRenameCall)?;
        phase = StagePhase::UncertainRename;
        if let Err(error) = rename(
            anchors.parent(),
            &temporary,
            #[cfg(test)]
            &mut hooks.files,
        ) {
            if error.raw_os_error().is_some_and(|code| {
                [
                    libc::EEXIST,
                    libc::ENOENT,
                    libc::ENOTDIR,
                    libc::EACCES,
                    libc::EPERM,
                    libc::EROFS,
                    libc::EINVAL,
                    libc::EXDEV,
                ]
                .contains(&code)
            }) {
                phase = StagePhase::BeforeRename;
            }
            return Err(UNSAFE);
        }
        phase = StagePhase::AppliedRename;
        #[cfg(test)]
        hooks.files.at(Point::Renamed)?;
        anchors.transition(&[FINAL], false)?;
        descriptor.renamed(&anchors)?;
        #[cfg(test)]
        hooks.files.at(Point::BeforeParentSync)?;
        anchors.parent().sync_all().map_err(|_| UNSAFE)?;
        #[cfg(test)]
        hooks.files.at(Point::Complete)?;
        guard(
            owner,
            &mut agreement,
            &selected,
            &mut staged,
            #[cfg(test)]
            hooks,
        )?;
        descriptor.check(&anchors, FINAL)?;
        anchors.check(&[FINAL])?;
        Ok(RegistrationOutcome::Registered)
    }
    .await;
    result.map_err(|_| RegistrationFailure { phase, residue })
}
