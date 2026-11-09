use super::{InstallerLock, StageFailure, StagedRelease};
use crate::release_files::{
    AnchoredRelease, ArtifactFile, Fingerprint, NAMES, check_acl, directory_matches,
};
#[cfg(test)]
use crate::release_manifest::ArtifactName;
use crate::release_manifest::ReleaseArtifact;
use crate::release_verify::macos::{ValidatedTrust, rehash_held, verify_held};
use ring::rand::{SecureRandom, SystemRandom};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

mod anchor;
use anchor::{CreatedStage, Releases};

const UNSAFE: &str = "unsafe or changed release stage";
const COPY_FAILED: &str = "release stage copy failed";

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    BeforeCreate,
    Created,
    BeforeFileCreate(ArtifactName),
    FileCreated(ArtifactName),
    BeforeWrite(ArtifactName),
    AfterWrite(ArtifactName),
    BeforeFileSync(ArtifactName),
    FileClosed(ArtifactName),
    BeforeStageSync,
    BeforeReleasesSync,
    BeforeDestinationOpen,
    DestinationVerified,
}

#[cfg(test)]
type Check<'a> = &'a mut dyn FnMut(ArtifactName, &Path) -> Result<(), &'static str>;
#[cfg(test)]
type Fault<'a> = &'a mut dyn FnMut(Point, &Path) -> Result<(), &'static str>;

#[cfg(test)]
pub(super) struct Hooks<'a> {
    check: Option<Check<'a>>,
    fault: Option<Fault<'a>>,
    nonce: Option<[u8; 16]>,
}

#[cfg(test)]
impl<'a> Hooks<'a> {
    pub(super) fn with_check(check: Check<'a>) -> Self {
        Self {
            check: Some(check),
            ..Self::production()
        }
    }
    pub(super) fn production() -> Self {
        Self {
            check: None,
            fault: None,
            nonce: None,
        }
    }
    fn at(&mut self, point: Point, path: &Path) -> Result<(), &'static str> {
        if let Some(fault) = self.fault.as_mut() {
            fault(point, path)?;
        }
        Ok(())
    }
}

fn verify(
    held: &mut AnchoredRelease,
    trust: &ValidatedTrust,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<(), &'static str> {
    #[cfg(test)]
    if let Some(check) = hooks.check.as_mut() {
        return verify_held(held, trust, Some(&mut **check));
    }
    verify_held(
        held,
        trust,
        #[cfg(test)]
        None,
    )
}

fn identity(file: &File) -> Result<(u64, u64), &'static str> {
    let metadata = file.metadata().map_err(|_| UNSAFE)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn new_name(#[cfg(test)] hooks: &Hooks<'_>) -> Result<String, &'static str> {
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "stage name unavailable")?;
    #[cfg(test)]
    if let Some(nonce) = hooks.nonce {
        bytes = nonce;
    }
    Ok(format!(
        ".stage-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

pub(super) fn stage<'lock>(
    owner: &'lock InstallerLock,
    source: &Path,
    manifest: &[u8],
    expected_digest: &[u8; 32],
    certificate_sha1: &str,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<StagedRelease<'lock>, StageFailure> {
    let mut residue = None;
    let result = (|| {
        let trust = ValidatedTrust::new(manifest, expected_digest, certificate_sha1)?;
        owner.revalidate()?;
        let mut source = AnchoredRelease::open(source)?;
        verify(
            &mut source,
            &trust,
            #[cfg(test)]
            hooks,
        )?;
        let mut releases = Releases::open(owner)?;
        if source
            .private_directory_identities()?
            .contains(&identity(releases.directory())?)
        {
            return Err("release source overlaps destination directory");
        }
        let name = new_name(
            #[cfg(test)]
            hooks,
        )?;
        #[cfg(test)]
        let path = owner.layout().releases_directory().join(&name);
        #[cfg(test)]
        hooks.at(Point::BeforeCreate, &path)?;
        source.revalidate()?;
        releases.before_create()?;
        owner.revalidate()?;
        let name_c = CString::new(name.as_str()).map_err(|_| UNSAFE)?;
        if unsafe { libc::mkdirat(releases.directory().as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err("release stage creation failed");
        }
        residue = Some(name.clone());
        releases.after_create()?;
        let mut created = CreatedStage::open(releases, owner, &name)?;
        #[cfg(test)]
        hooks.at(Point::Created, &created.path)?;
        let mut copied = Vec::with_capacity(3);
        for (index, name) in NAMES.into_iter().enumerate() {
            source.revalidate()?;
            owner.revalidate()?;
            let artifact = trust
                .manifest()
                .artifacts()
                .iter()
                .find(|artifact| artifact.name == name)
                .ok_or("release artifact is missing")?;
            copied.push(copy_file(
                &mut created,
                &mut source.artifacts[index],
                artifact,
                #[cfg(test)]
                hooks,
            )?);
            source.revalidate()?;
        }
        created.revalidate(false)?;
        #[cfg(test)]
        hooks.at(Point::BeforeStageSync, &created.path)?;
        created
            .file
            .sync_all()
            .map_err(|_| "release stage directory sync failed")?;
        #[cfg(test)]
        hooks.at(Point::BeforeReleasesSync, &created.path)?;
        created
            .releases
            .directory()
            .sync_all()
            .map_err(|_| "releases directory sync failed")?;
        created.revalidate(false)?;
        #[cfg(test)]
        hooks.at(Point::BeforeDestinationOpen, &created.path)?;
        let mut destination = AnchoredRelease::open(&created.path)?;
        if destination.private_directory_identities()?
            != [
                identity(&created.file)?,
                identity(created.releases.directory())?,
            ]
        {
            return Err(UNSAFE);
        }
        for (file, copied) in destination.artifacts.iter().zip(copied) {
            if file.fingerprint != copied {
                return Err(UNSAFE);
            }
        }
        verify(
            &mut destination,
            &trust,
            #[cfg(test)]
            hooks,
        )?;
        #[cfg(test)]
        hooks.at(Point::DestinationVerified, &created.path)?;
        rehash_held(&mut source, &trust)?;
        rehash_held(&mut destination, &trust)?;
        source.revalidate()?;
        destination.revalidate()?;
        created.revalidate(false)?;
        owner.revalidate()?;
        Ok(StagedRelease {
            _owner: owner,
            _destination: destination,
            _canonical_manifest: manifest.into(),
            _trust: trust,
            directory_name: name,
        })
    })();
    result.map_err(|reason| StageFailure { reason, residue })
}

fn file_policy(file: &File, mode: u32) -> Result<Fingerprint, &'static str> {
    let metadata = file.metadata().map_err(|_| UNSAFE)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != mode
        || metadata.nlink() != 1
    {
        return Err(UNSAFE);
    }
    check_acl(file.as_raw_fd(), true)?;
    Ok(metadata.into())
}

fn copy_file(
    stage: &mut CreatedStage,
    source: &mut ArtifactFile,
    artifact: &ReleaseArtifact,
    #[cfg(test)] hooks: &mut Hooks<'_>,
) -> Result<Fingerprint, &'static str> {
    stage.revalidate(false)?;
    #[cfg(test)]
    hooks.at(Point::BeforeFileCreate(artifact.name), &stage.path)?;
    let name = CString::new(artifact.name.filename()).map_err(|_| UNSAFE)?;
    let fd = unsafe {
        libc::openat(
            stage.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(COPY_FAILED);
    }
    let mut writer = unsafe { File::from_raw_fd(fd) };
    let initial = file_policy(&writer, 0o600)?;
    if initial.size != 0 {
        return Err(UNSAFE);
    }
    stage.after_file_created()?;
    #[cfg(test)]
    hooks.at(Point::FileCreated(artifact.name), &stage.path)?;
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| COPY_FAILED)?;
    let mut reader = (&mut source.file).take(artifact.size + 1);
    let mut buffer = [0; 64 * 1024];
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut size = 0;
    #[cfg(test)]
    hooks.at(Point::BeforeWrite(artifact.name), &stage.path)?;
    stage.revalidate(false)?;
    if file_policy(&writer, 0o600)? != initial {
        return Err(UNSAFE);
    }
    loop {
        let read = reader.read(&mut buffer).map_err(|_| COPY_FAILED)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > artifact.size {
            return Err(COPY_FAILED);
        }
        digest.update(&buffer[..read]);
        writer.write_all(&buffer[..read]).map_err(|_| COPY_FAILED)?;
    }
    let actual: String = digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if size != artifact.size || actual != artifact.sha256 {
        return Err(COPY_FAILED);
    }
    #[cfg(test)]
    hooks.at(Point::AfterWrite(artifact.name), &stage.path)?;
    let copied = file_policy(&writer, 0o600)?;
    if copied.size != artifact.size || !directory_matches(&copied, &initial, false) {
        return Err(UNSAFE);
    }
    if unsafe { libc::fchmod(writer.as_raw_fd(), 0o500) } != 0 {
        return Err(COPY_FAILED);
    }
    #[cfg(test)]
    hooks.at(Point::BeforeFileSync(artifact.name), &stage.path)?;
    writer
        .sync_all()
        .map_err(|_| "release stage file sync failed")?;
    let final_fingerprint = file_policy(&writer, 0o500)?;
    drop(writer);
    #[cfg(test)]
    hooks.at(Point::FileClosed(artifact.name), &stage.path)?;
    stage.revalidate(false)?;
    Ok(final_fingerprint)
}

#[cfg(test)]
mod tests;
