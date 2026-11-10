//! Protected, independently pinned input shared by release diagnostics and publication.

use handkite_host::{release_manifest::MAX_MANIFEST_BYTES, release_verify};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

const HEADER_BYTES: usize = 65 + 41;
const MAX_INPUT_BYTES: usize = HEADER_BYTES + MAX_MANIFEST_BYTES;

pub(super) fn stage_arg(mut args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    if args.next()?.as_os_str() != "--stage" {
        return None;
    }
    let stage = PathBuf::from(args.next()?);
    (args.next().is_none() && stage.is_absolute()).then_some(stage)
}

pub(super) fn read_input(input: &mut File) -> Option<Vec<u8>> {
    let metadata = input.metadata().ok()?;
    // This is bounded input admission, not independent trust in either pin.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() <= HEADER_BYTES as u64
        || metadata.len() > MAX_INPUT_BYTES as u64
    {
        return None;
    }
    release_verify::check_private_input_acl(input).ok()?;
    let mut bytes = Vec::new();
    input
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

pub(super) struct Envelope<'a> {
    pub(super) manifest: &'a [u8],
    pub(super) digest: [u8; 32],
    pub(super) certificate: &'a str,
}

pub(super) fn envelope(bytes: &[u8]) -> Option<Envelope<'_>> {
    if bytes.len() <= HEADER_BYTES
        || bytes.len() > MAX_INPUT_BYTES
        || bytes[64] != b'\n'
        || bytes[105] != b'\n'
    {
        return None;
    }
    let mut digest = [0; 32];
    for (output, pair) in digest.iter_mut().zip(bytes[..64].chunks_exact(2)) {
        *output = hex_digit(pair[0])? * 16 + hex_digit(pair[1])?;
    }
    let certificate = &bytes[65..105];
    if !certificate.iter().all(|byte| hex_digit(*byte).is_some()) {
        return None;
    }
    Some(Envelope {
        manifest: &bytes[HEADER_BYTES..],
        digest,
        certificate: std::str::from_utf8(certificate).ok()?,
    })
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
