use super::*;
use crate::release_files::{AnchoredRelease, Fingerprint, NAMES, check_acl, open_at};
use crate::release_manifest::MAX_ARTIFACT_BYTES;
use crate::release_signer::{RunError, run_command};
use crate::release_verify::macos::{ValidatedTrust, verify_held};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::process::{Command, ExitStatus};
use std::time::Duration;

mod anchor;
use anchor::{Anchor, HeldFile, create_file, file_policy, identity, inventory, mkdir};

const UNSAFE: &str = "unsafe or changed package files";
const SIGN_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
type TestRun<'a> = &'a mut dyn FnMut(&mut Command, Duration) -> Result<ExitStatus, RunError>;

struct Runner<'a> {
    #[cfg(test)]
    fake: Option<TestRun<'a>>,
    lifetime: std::marker::PhantomData<&'a ()>,
}

impl Runner<'_> {
    fn run(&mut self, command: &mut Command, timeout: Duration) -> Result<ExitStatus, RunError> {
        #[cfg(test)]
        if let Some(fake) = &mut self.fake {
            return fake(command, timeout);
        }
        run_command(command, timeout)
    }
}

fn sign(
    path: &Path,
    name: ArtifactName,
    certificate: &str,
    runner: &mut Runner<'_>,
) -> Result<(), &'static str> {
    SignerPolicy::new(certificate, name)?;
    if !path.is_absolute() {
        return Err(UNSAFE);
    }
    let mut command = Command::new("/usr/bin/codesign");
    command
        .env_clear()
        .args([
            "--force",
            "--sign",
            certificate,
            "--identifier",
            name.code_identifier(),
            "--timestamp=none",
            "--",
        ])
        .arg(path);
    match runner.run(&mut command, SIGN_TIMEOUT) {
        Ok(status) if status.success() => Ok(()),
        _ => Err("package signing failed"),
    }
}

fn signature(
    path: &Path,
    name: ArtifactName,
    certificate: &str,
    runner: &mut Runner<'_>,
) -> Result<(), &'static str> {
    SignerPolicy::new(certificate, name)?
        .check_with(path, |command, timeout| runner.run(command, timeout))
}

pub(super) fn package(
    source_path: &Path,
    output: &Path,
    request: &PackageRequest,
    #[cfg(test)] fake: Option<TestRun<'_>>,
) -> Result<PackagedRelease, PackageFailure> {
    let mut phase = PackagePhase::Admission;
    let mut residue = false;
    let mut runner = Runner {
        #[cfg(test)]
        fake,
        lifetime: std::marker::PhantomData,
    };
    let result = (|| {
        let output_components = crate::release_files::components(output)?;
        let output_name = output_components.last().ok_or(UNSAFE)?;
        let parent_path = output.parent().ok_or(UNSAFE)?;
        let source = Anchor::open(source_path, false)?;
        let mut parent = Anchor::open(parent_path, true)?;
        if identity(source.file())? == identity(parent.file())? {
            return Err(UNSAFE);
        }
        let mut sources = Vec::with_capacity(3);
        for name in NAMES {
            let held = HeldFile::open(source.file(), &filename(name), None)?;
            sources.push(held);
        }
        check_sources(&source, &mut sources, request)?;
        parent.check(false)?;
        mkdir(parent.file(), output_name)?;
        residue = true;
        phase = PackagePhase::Copy;
        parent.refresh()?;
        let mut root = Anchor::open(output, true)?;
        inventory(root.file(), &[])?;
        mkdir(root.file(), c"artifacts")?;
        root.refresh()?;
        inventory(root.file(), &["artifacts"])?;
        let stage_path = output.join("artifacts");
        let mut stage = Anchor::open(&stage_path, true)?;
        inventory(stage.file(), &[])?;
        let mut copies = Vec::with_capacity(3);
        for (index, name) in NAMES.into_iter().enumerate() {
            check_sources(&source, &mut sources, request)?;
            check_output(&parent, &root, &stage, &copies, None)?;
            let candidate = candidate(request, name)?;
            let mut writer = create_file(stage.file(), &filename(name))?;
            stage.refresh()?;
            inventory(stage.file(), &artifact_names(&NAMES[..=index]))?;
            copy_bytes(&mut sources[index].file, &mut writer, candidate)?;
            if unsafe { libc::fchmod(writer.as_raw_fd(), 0o700) } != 0 {
                return Err(UNSAFE);
            }
            writer.sync_all().map_err(|_| UNSAFE)?;
            let fingerprint = file_policy(&writer, 0o700)?;
            drop(writer);
            let held = HeldFile::open(stage.file(), &filename(name), Some(0o700))?;
            if held.fingerprint != fingerprint {
                return Err(UNSAFE);
            }
            copies.push(held);
        }
        check_sources(&source, &mut sources, request)?;
        check_output(&parent, &root, &stage, &copies, None)?;
        phase = PackagePhase::Signing;
        for (index, name) in NAMES.into_iter().enumerate() {
            check_sources(&source, &mut sources, request)?;
            check_output(&parent, &root, &stage, &copies, None)?;
            let path = stage_path.join(name.filename());
            sign(&path, name, &request.certificate, &mut runner)?;
            // Apple's Mach-O signer commits by rename. Only this active copy
            // may be replaced; its unlinked original and all sources stay fixed.
            copies[index].check_replaced()?;
            check_hash(&mut copies[index].file, candidate(request, name)?)?;
            stage.refresh()?;
            check_output(&parent, &root, &stage, &copies, Some(index))?;
            let signed = HeldFile::open(stage.file(), &filename(name), Some(0o700))?;
            if identity(&signed.file)? == identity(&copies[index].file)? {
                return Err(UNSAFE);
            }
            copies[index] = signed;
            check_macho(&mut copies[index].file)?;
            signature(&path, name, &request.certificate, &mut runner)?;
            check_output(&parent, &root, &stage, &copies, None)?;
            check_sources(&source, &mut sources, request)?;
            copies[index].freeze(0o500)?;
            check_output(&parent, &root, &stage, &copies, None)?;
        }
        phase = PackagePhase::Inventory;
        let mut artifacts = Vec::with_capacity(3);
        for (name, held) in NAMES.into_iter().zip(&mut copies) {
            let size = held.fingerprint.size;
            let sha256 = hash_file(&mut held.file, size)?;
            artifacts.push(ReleaseArtifact::new(name, size, sha256));
        }
        let manifest = ReleaseManifest::new(
            request.candidates.source_sha().into(),
            RustIdentity {
                toolchain: request.candidates.rust().toolchain.clone(),
                compiler_commit: request.candidates.rust().compiler_commit.clone(),
            },
            SdkIdentity {
                version: request.candidates.sdk().version.clone(),
                build: request.candidates.sdk().build.clone(),
            },
            artifacts.try_into().map_err(|_| UNSAFE)?,
        )?;
        let bytes = manifest.canonical_json()?;
        let digest = manifest.canonical_sha256()?;
        check_output(&parent, &root, &stage, &copies, None)?;
        let mut writer = create_file(root.file(), c"manifest.json")?;
        root.refresh()?;
        writer.write_all(&bytes).map_err(|_| UNSAFE)?;
        writer.sync_all().map_err(|_| UNSAFE)?;
        let written = file_policy(&writer, 0o600)?;
        drop(writer);
        let mut receipt = HeldFile::open(root.file(), c"manifest.json", Some(0o600))?;
        if receipt.fingerprint != written {
            return Err(UNSAFE);
        }
        phase = PackagePhase::Verification;
        // This generated digest checks internal consistency only. Publication
        // must still receive its manifest pin from independent operator approval.
        let trust = ValidatedTrust::new(&bytes, &digest, &request.certificate)?;
        let mut held = AnchoredRelease::open(&stage_path)?;
        for (verified, copied) in held.artifacts.iter().zip(&copies) {
            if verified.fingerprint != copied.fingerprint {
                return Err(UNSAFE);
            }
        }
        #[cfg(test)]
        let mut check =
            |name, path: &Path| signature(path, name, &request.certificate, &mut runner);
        verify_held(
            &mut held,
            &trust,
            #[cfg(test)]
            Some(&mut check),
        )?;
        check_sources(&source, &mut sources, request)?;
        check_output(&parent, &root, &stage, &copies, None)?;
        inventory(root.file(), &["artifacts", "manifest.json"])?;
        receipt.check(root.file(), c"manifest.json")?;
        if hash_file(&mut receipt.file, bytes.len() as u64)? != hex(&digest) {
            return Err(UNSAFE);
        }
        stage.file().sync_all().map_err(|_| UNSAFE)?;
        root.file().sync_all().map_err(|_| UNSAFE)?;
        parent.file().sync_all().map_err(|_| UNSAFE)?;
        check_sources(&source, &mut sources, request)?;
        held.revalidate()?;
        check_output(&parent, &root, &stage, &copies, None)?;
        receipt.check(root.file(), c"manifest.json")?;
        Ok(PackagedRelease {
            manifest_sha256: digest,
        })
    })();
    result.map_err(|_: &'static str| PackageFailure { phase, residue })
}

fn filename(name: ArtifactName) -> CString {
    CString::new(name.filename()).expect("fixed artifact name")
}

fn artifact_names(names: &[ArtifactName]) -> Vec<&'static str> {
    names.iter().map(|name| name.filename()).collect()
}

fn candidate(
    request: &PackageRequest,
    name: ArtifactName,
) -> Result<&ReleaseArtifact, &'static str> {
    request
        .candidates
        .artifacts()
        .iter()
        .find(|item| item.name == name)
        .ok_or(UNSAFE)
}

fn check_sources(
    source: &Anchor,
    files: &mut [HeldFile],
    request: &PackageRequest,
) -> Result<(), &'static str> {
    source.check(false)?;
    for (name, held) in NAMES.into_iter().zip(files) {
        held.check(source.file(), &filename(name))?;
        check_macho(&mut held.file)?;
        check_hash(&mut held.file, candidate(request, name)?)?;
        held.check(source.file(), &filename(name))?;
    }
    source.check(false)
}

fn check_output(
    parent: &Anchor,
    root: &Anchor,
    stage: &Anchor,
    files: &[HeldFile],
    replacing: Option<usize>,
) -> Result<(), &'static str> {
    parent.check(false)?;
    root.check(false)?;
    stage.check(false)?;
    inventory(stage.file(), &artifact_names(&NAMES[..files.len()]))?;
    for (index, (name, held)) in NAMES.into_iter().zip(files).enumerate() {
        if Some(index) != replacing {
            held.check(stage.file(), &filename(name))?;
        }
    }
    Ok(())
}

fn check_macho(file: &mut File) -> Result<(), &'static str> {
    file.seek(SeekFrom::Start(0)).map_err(|_| UNSAFE)?;
    let mut header = [0; 32];
    file.read_exact(&mut header).map_err(|_| UNSAFE)?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || u32::from_le_bytes(header[4..8].try_into().unwrap()) != 0x0100_000c
        || u32::from_le_bytes(header[12..16].try_into().unwrap()) != 2
    {
        return Err(UNSAFE);
    }
    Ok(())
}

fn check_hash(file: &mut File, expected: &ReleaseArtifact) -> Result<(), &'static str> {
    if hash_file(file, expected.size)? != expected.sha256 {
        return Err(UNSAFE);
    }
    Ok(())
}

fn hash_file(file: &mut File, size: u64) -> Result<String, &'static str> {
    digest_bytes(file, size, None)
}

fn copy_bytes(
    source: &mut File,
    writer: &mut File,
    expected: &ReleaseArtifact,
) -> Result<(), &'static str> {
    if digest_bytes(source, expected.size, Some(writer))? != expected.sha256 {
        return Err(UNSAFE);
    }
    Ok(())
}

fn digest_bytes(
    file: &mut File,
    size: u64,
    mut writer: Option<&mut File>,
) -> Result<String, &'static str> {
    if !(1..=MAX_ARTIFACT_BYTES).contains(&size) {
        return Err(UNSAFE);
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| UNSAFE)?;
    let mut reader = file.take(size + 1);
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; 64 * 1024];
    let mut count = 0;
    loop {
        let read = reader.read(&mut buffer).map_err(|_| UNSAFE)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > size {
            return Err(UNSAFE);
        }
        digest.update(&buffer[..read]);
        if let Some(writer) = &mut writer {
            writer.write_all(&buffer[..read]).map_err(|_| UNSAFE)?;
        }
    }
    if count != size {
        return Err(UNSAFE);
    }
    Ok(hex(digest.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
