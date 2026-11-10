use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use zeroize::Zeroizing;

const INVALID: &str = "developer credential input is unsafe or invalid";
const MAX_BYTES: usize = 4354;

/// One account identity and Xcode token from a private, one-shot input file.
/// The source is never reopened, logged, or persisted by this module.
pub struct DeveloperCredential {
    adsid: Zeroizing<String>,
    token: Zeroizing<String>,
}

impl DeveloperCredential {
    pub fn read_once(mut file: File) -> Result<Self, &'static str> {
        let before = file.metadata().map_err(|_| INVALID)?;
        if !before.is_file()
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o7777 != 0o600
            || before.nlink() != 1
            || before.len() == 0
            || before.len() > MAX_BYTES as u64
        {
            return Err(INVALID);
        }
        crate::release_verify::check_private_input_acl(&file).map_err(|_| INVALID)?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.by_ref()
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| INVALID)?;
        let after = file.metadata().map_err(|_| INVALID)?;
        if bytes.len() as u64 != before.len()
            || after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.len() != before.len()
            || after.mode() != before.mode()
            || after.nlink() != before.nlink()
        {
            return Err(INVALID);
        }
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut lines = bytes.split(|byte| *byte == b'\n');
        let (Some(adsid), Some(token), Some(b""), None) =
            (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            return Err(INVALID);
        };
        if adsid.is_empty()
            || adsid.len() > 256
            || !adsid.iter().all(u8::is_ascii_digit)
            || token.is_empty()
            || token.len() > 4096
            || !token.iter().all(|byte| (0x21..=0x7e).contains(byte))
        {
            return Err(INVALID);
        }
        Ok(Self {
            adsid: Zeroizing::new(std::str::from_utf8(adsid).map_err(|_| INVALID)?.to_owned()),
            token: Zeroizing::new(std::str::from_utf8(token).map_err(|_| INVALID)?.to_owned()),
        })
    }

    pub(crate) fn adsid(&self) -> &str {
        &self.adsid
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    #[test]
    fn strict_two_line_envelope_has_no_debug_surface() {
        let valid = DeveloperCredential::parse(b"12345\nsynthetic-token\n").unwrap();
        assert_eq!(valid.adsid(), "12345");
        assert_eq!(valid.token(), "synthetic-token");
        for bytes in [
            &b"12345\nsynthetic-token"[..],
            b"12345\nsynthetic-token\nextra\n",
            b"12x45\nsynthetic-token\n",
            b"12345\nsynthetic token\n",
            b"12345\r\nsynthetic-token\n",
        ] {
            assert!(DeveloperCredential::parse(bytes).is_err());
        }
    }

    #[test]
    fn private_regular_file_is_read_once() {
        let mut input = NamedTempFile::new().unwrap();
        input.write_all(b"12345\nsynthetic-token\n").unwrap();
        input.seek(SeekFrom::Start(0)).unwrap();
        let credential = DeveloperCredential::read_once(input.reopen().unwrap()).unwrap();
        assert_eq!(credential.adsid(), "12345");
    }

    #[test]
    fn pipe_and_bad_mode_are_rejected() {
        let null = File::open("/dev/null").unwrap();
        assert!(DeveloperCredential::read_once(null).is_err());
        let input = NamedTempFile::new().unwrap();
        input
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(DeveloperCredential::read_once(input.reopen().unwrap()).is_err());
    }
}
