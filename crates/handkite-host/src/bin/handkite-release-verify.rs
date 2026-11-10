use handkite_host::{release_manifest::MAX_MANIFEST_BYTES, release_verify};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HEADER_BYTES: usize = 65 + 41;
const MAX_INPUT_BYTES: usize = HEADER_BYTES + MAX_MANIFEST_BYTES;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Reject,
    Invalid,
}

impl Outcome {
    fn report(&self) -> (u8, &'static [u8]) {
        match self {
            Self::Pass => (0, b"release_verify=pass\n"),
            Self::Reject => (1, b"release_verify=reject\n"),
            Self::Invalid => (2, b"release_verify=invalid\n"),
        }
    }
}

fn main() -> ExitCode {
    let outcome = match io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => run(
            std::env::args_os().skip(1),
            &mut File::from(fd),
            release_verify::verify,
        ),
        Err(_) => Outcome::Invalid,
    };
    let (code, output) = outcome.report();
    if io::stdout().lock().write_all(output).is_err() {
        return ExitCode::from(2);
    }
    ExitCode::from(code)
}

fn stage_arg(mut args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    if args.next()?.as_os_str() != "--stage" {
        return None;
    }
    let stage = PathBuf::from(args.next()?);
    (args.next().is_none() && stage.is_absolute()).then_some(stage)
}

fn read_input(input: &mut File) -> Option<Vec<u8>> {
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
    let mut bytes = Vec::new();
    input
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

struct Envelope<'a> {
    manifest: &'a [u8],
    digest: [u8; 32],
    certificate: &'a str,
}

fn envelope(bytes: &[u8]) -> Option<Envelope<'_>> {
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

fn run(
    args: impl Iterator<Item = OsString>,
    input: &mut File,
    verify: impl FnOnce(&Path, &[u8], &[u8; 32], &str) -> Result<(), &'static str>,
) -> Outcome {
    let Some(stage) = stage_arg(args) else {
        return Outcome::Invalid;
    };
    let Some(bytes) = read_input(input) else {
        return Outcome::Invalid;
    };
    let Some(envelope) = envelope(&bytes) else {
        return Outcome::Invalid;
    };
    match verify(
        &stage,
        envelope.manifest,
        &envelope.digest,
        envelope.certificate,
    ) {
        Ok(()) => Outcome::Pass,
        Err(_) => Outcome::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CERTIFICATE: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn payload(manifest: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{DIGEST}\n{CERTIFICATE}\n").into_bytes();
        bytes.extend_from_slice(manifest);
        bytes
    }

    fn input(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values.iter().map(OsString::from)
    }

    #[test]
    fn accepts_only_exact_argument_grammar() {
        for values in [
            vec![],
            vec!["--stage"],
            vec!["--help"],
            vec!["--stage=/stage"],
            vec!["--stage", "stage"],
            vec!["--stage", ""],
            vec!["--stage", "/stage", "extra"],
            vec!["--stage", "/stage", "--stage", "/other"],
            vec!["--config", "/stage"],
        ] {
            assert!(stage_arg(args(&values)).is_none());
        }
        assert_eq!(
            stage_arg(args(&["--stage", "/stage"])).unwrap(),
            Path::new("/stage")
        );
    }

    #[test]
    fn preserves_path_bytes_without_normalization() {
        let raw = b"/stage/../private//\xff";
        let stage =
            stage_arg(["--stage".into(), OsString::from_vec(raw.to_vec())].into_iter()).unwrap();
        assert_eq!(stage.as_os_str().as_bytes(), raw);
    }

    #[test]
    fn decodes_exact_pins_and_preserves_manifest_bytes() {
        let bytes = payload(b"\x00\xff\r\n{}\n");
        let parsed = envelope(&bytes).unwrap();
        assert_eq!(parsed.manifest, b"\x00\xff\r\n{}\n");
        assert_eq!(parsed.certificate, CERTIFICATE);
        assert_eq!(
            parsed.digest,
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
                .repeat(4)
                .as_slice()
        );
    }

    #[test]
    fn rejects_malformed_pins_and_line_delimiters() {
        let valid = payload(b"{}");
        for index in [0, 63, 65, 104] {
            for invalid in [b'A', b'g', b' ', b'\r', 0, 0xff] {
                let mut bytes = valid.clone();
                bytes[index] = invalid;
                assert!(envelope(&bytes).is_none());
            }
        }
        for index in [64, 105] {
            for invalid in [b'\r', b' ', b'0'] {
                let mut bytes = valid.clone();
                bytes[index] = invalid;
                assert!(envelope(&bytes).is_none());
            }
            let mut bytes = valid.clone();
            bytes.insert(index, b'\r');
            assert!(envelope(&bytes).is_none());
        }
    }

    #[test]
    fn enforces_nonempty_manifest_and_exact_size_bound() {
        let empty_manifest = payload(b"");
        for length in 0..=empty_manifest.len() {
            assert!(envelope(&empty_manifest[..length]).is_none());
        }
        assert!(envelope(&payload(&vec![b'x'; MAX_MANIFEST_BYTES])).is_some());
        assert!(envelope(&payload(&vec![b'x'; MAX_MANIFEST_BYTES + 1])).is_none());
    }

    #[test]
    fn admits_private_regular_file_and_rejects_unsafe_modes() {
        let bytes = payload(b"{}");
        for mode in [0o400, 0o640, 0o666, 0o700, 0o4600] {
            let mut file = input(&bytes);
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .unwrap();
            assert!(read_input(file.as_file_mut()).is_none());
        }
        let mut file = input(&bytes);
        assert_eq!(read_input(file.as_file_mut()).unwrap(), bytes);
    }

    #[test]
    fn rejects_nonregular_or_multiple_link_input() {
        let mut device = File::open("/dev/null").unwrap();
        assert!(read_input(&mut device).is_none());
        let directory = tempfile::tempdir().unwrap();
        let mut file = input(&payload(b"{}"));
        std::fs::hard_link(file.path(), directory.path().join("linked")).unwrap();
        assert!(read_input(file.as_file_mut()).is_none());
    }

    #[test]
    fn checks_file_size_before_reading() {
        for bytes in [payload(b""), payload(&vec![b'x'; MAX_MANIFEST_BYTES + 1])] {
            let mut file = input(&bytes);
            assert!(read_input(file.as_file_mut()).is_none());
            assert_eq!(file.stream_position().unwrap(), 0);
        }
        let bytes = payload(&vec![b'x'; MAX_MANIFEST_BYTES]);
        let mut file = input(&bytes);
        assert_eq!(read_input(file.as_file_mut()).unwrap(), bytes);
    }

    #[test]
    fn calls_verifier_once_with_unchanged_inputs() {
        let raw_stage = b"/private/../stage//\xff";
        let raw_manifest = b"\xff{\n}\n";
        let mut file = input(&payload(raw_manifest));
        let mut calls = 0;
        let outcome = run(
            ["--stage".into(), OsString::from_vec(raw_stage.to_vec())].into_iter(),
            file.as_file_mut(),
            |stage, manifest, digest, certificate| {
                calls += 1;
                assert_eq!(stage.as_os_str().as_bytes(), raw_stage);
                assert_eq!(manifest, raw_manifest);
                assert_eq!(
                    digest,
                    envelope(&payload(raw_manifest)).unwrap().digest.as_ref()
                );
                assert_eq!(certificate, CERTIFICATE);
                Ok(())
            },
        );
        assert_eq!(outcome, Outcome::Pass);
        assert_eq!(calls, 1);
    }

    #[test]
    fn invalid_inputs_never_call_verifier() {
        let mut file = input(&payload(b"{}"));
        assert_eq!(
            run(
                args(&["--stage", "relative"]),
                file.as_file_mut(),
                |_, _, _, _| panic!("unexpected verification")
            ),
            Outcome::Invalid
        );
        let mut file = input(b"malformed");
        assert_eq!(
            run(
                args(&["--stage", "/stage"]),
                file.as_file_mut(),
                |_, _, _, _| panic!("unexpected verification")
            ),
            Outcome::Invalid
        );
    }

    #[test]
    fn fixed_reports_discard_verifier_errors() {
        let mut file = input(&payload(b"{}"));
        let outcome = run(
            args(&["--stage", "/stage"]),
            file.as_file_mut(),
            |_, _, _, _| Err("private upstream detail"),
        );
        assert_eq!(outcome.report(), (1, b"release_verify=reject\n".as_slice()));
        assert_eq!(
            Outcome::Pass.report(),
            (0, b"release_verify=pass\n".as_slice())
        );
        assert_eq!(
            Outcome::Invalid.report(),
            (2, b"release_verify=invalid\n".as_slice())
        );
    }
}
