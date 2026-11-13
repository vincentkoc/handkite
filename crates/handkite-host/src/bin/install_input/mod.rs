//! Shared protected release and independent binding input for installer commands.

use super::{binding_input, release_input};
use handkite_core::protocol::DeviceBinding;
use handkite_host::{
    release_manifest::{ArtifactName, MAX_MANIFEST_BYTES, ReleaseManifest},
    release_signer::SignerPolicy,
    release_verify::check_private_input_acl,
};
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::Seek;
use std::os::unix::fs::MetadataExt;

#[allow(dead_code)] // The update command has its own explicit operation/fd arguments.
pub(super) fn arguments(mut args: impl Iterator<Item = OsString>) -> bool {
    args.next().as_deref() == Some(std::ffi::OsStr::new("--binding-fd"))
        && args.next().as_deref() == Some(std::ffi::OsStr::new("3"))
        && args.next().is_none()
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    (
        left.dev(),
        left.ino(),
        left.uid(),
        left.gid(),
        left.mode(),
        left.nlink(),
        left.len(),
        left.mtime(),
        left.mtime_nsec(),
        left.ctime(),
        left.ctime_nsec(),
    ) == (
        right.dev(),
        right.ino(),
        right.uid(),
        right.gid(),
        right.mode(),
        right.nlink(),
        right.len(),
        right.mtime(),
        right.mtime_nsec(),
        right.ctime(),
        right.ctime_nsec(),
    )
}

pub(super) fn protected_read(
    input: &mut File,
    max: usize,
    read: impl FnOnce(&mut File) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let before = input.metadata().ok()?;
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > max as u64
        || input.stream_position().ok()? != 0
    {
        return None;
    }
    check_private_input_acl(input).ok()?;
    let bytes = read(input)?;
    if bytes.len() as u64 != before.len() || !same_metadata(&before, &input.metadata().ok()?) {
        return None;
    }
    check_private_input_acl(input).ok()?;
    Some(bytes)
}

#[allow(dead_code)] // Initial selection and descriptor staging use two inputs.
pub(super) fn run<R>(
    input: &mut File,
    expected: &mut File,
    stage: impl FnOnce(release_input::Envelope<'_>, &DeviceBinding) -> R,
) -> Option<R> {
    let left = input.metadata().ok()?;
    let right = expected.metadata().ok()?;
    if (left.dev(), left.ino()) == (right.dev(), right.ino()) {
        return None;
    }
    let bytes = release_bytes(input)?;
    let pins = release_input::envelope(&bytes)?;
    let expected_binding = binding_input::read_binding(expected)?;
    if !expected
        .metadata()
        .is_ok_and(|after| same_metadata(&right, &after))
        || check_private_input_acl(expected).is_err()
    {
        return None;
    }
    Some(stage(pins, &expected_binding))
}

fn release_bytes(input: &mut File) -> Option<Vec<u8>> {
    let bytes = protected_read(input, MAX_MANIFEST_BYTES + 106, release_input::read_input)?;
    let pins = release_input::envelope(&bytes)?;
    let manifest = ReleaseManifest::parse(pins.manifest).ok()?;
    if manifest.canonical_json().ok().as_deref() != Some(pins.manifest)
        || ring::digest::digest(&ring::digest::SHA256, pins.manifest).as_ref() != pins.digest
        || SignerPolicy::new(pins.certificate, ArtifactName::Daemon).is_err()
    {
        return None;
    }
    Some(bytes)
}

#[allow(dead_code)] // Only the update command consumes a previous release pin.
pub(super) fn run_update<R>(
    destination: &mut File,
    binding: &mut File,
    previous: &mut File,
    execute: impl FnOnce(release_input::Envelope<'_>, release_input::Envelope<'_>, &DeviceBinding) -> R,
) -> Option<R> {
    let before = [
        destination.metadata().ok()?,
        binding.metadata().ok()?,
        previous.metadata().ok()?,
    ];
    for left in 0..before.len() {
        for right in left + 1..before.len() {
            if (before[left].dev(), before[left].ino())
                == (before[right].dev(), before[right].ino())
            {
                return None;
            }
        }
    }
    let destination_bytes = release_bytes(destination)?;
    let previous_bytes = release_bytes(previous)?;
    let destination_pin = release_input::envelope(&destination_bytes)?;
    let previous_pin = release_input::envelope(&previous_bytes)?;
    if destination_pin.digest == previous_pin.digest {
        return None;
    }
    let expected = binding_input::read_binding(binding)?;
    for (file, original) in [destination, binding, previous].into_iter().zip(&before) {
        if !file
            .metadata()
            .is_ok_and(|after| same_metadata(original, &after))
            || check_private_input_acl(file).is_err()
        {
            return None;
        }
    }
    Some(execute(destination_pin, previous_pin, &expected))
}
