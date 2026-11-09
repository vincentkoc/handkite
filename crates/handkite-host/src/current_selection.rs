//! Pure codec for the relative symlink target at `releases/current`.
//!
//! Syntax is not trust. A future transaction must independently verify the
//! manifest pin, anchored symlink and destination, and ownership before use.
//! These functions perform no filesystem access or activation.

use crate::install_plan::ManifestDigest;

/// Encode exactly one lowercase hexadecimal manifest-digest directory name.
pub fn encode_target(manifest: &ManifestDigest) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut target = [0; 64];
    for (byte, pair) in manifest.iter().zip(target.chunks_exact_mut(2)) {
        pair[0] = HEX[(byte >> 4) as usize];
        pair[1] = HEX[(byte & 15) as usize];
    }
    target
}

/// Parse raw target bytes without trimming, Unicode conversion, or normalization.
pub fn parse_target(target: &[u8]) -> Result<ManifestDigest, &'static str> {
    const INVALID: &str = "invalid current-release target";
    if target.len() != 64 {
        return Err(INVALID);
    }
    let nibble = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(INVALID),
    };
    let mut manifest = [0; 32];
    for (pair, byte) in target.chunks_exact(2).zip(&mut manifest) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_encoding_and_all_byte_values_round_trip() {
        let manifest = std::array::from_fn(|index| index as u8);
        assert_eq!(
            encode_target(&manifest),
            *b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
        assert_eq!(parse_target(&encode_target(&manifest)), Ok(manifest));
        for byte in 0..=u8::MAX {
            let manifest = [byte; 32];
            assert_eq!(parse_target(&encode_target(&manifest)), Ok(manifest));
        }
    }

    #[test]
    fn rejects_lengths_prefixes_components_and_alternate_spellings() {
        let valid = encode_target(&[0xab; 32]);
        for invalid in [
            Vec::new(),
            valid[..63].to_vec(),
            [valid.as_slice(), b"0"].concat(),
            [b"0x", valid.as_slice()].concat(),
            [b"./", valid.as_slice()].concat(),
            [b"../", valid.as_slice()].concat(),
            [b"/", valid.as_slice()].concat(),
            [valid.as_slice(), b"/"].concat(),
            [valid.as_slice(), b"\n"].concat(),
            vec![b'A'; 64],
            vec![b'.'; 64],
            vec![b' '; 64],
            vec![0; 64],
            vec![0xff; 64],
            vec![b'a'; 4096],
        ] {
            assert!(parse_target(&invalid).is_err());
        }
    }

    #[test]
    fn every_byte_at_every_target_position_obeys_the_exact_grammar() {
        for index in 0..64 {
            for byte in 0..=u8::MAX {
                let mut target = [b'0'; 64];
                target[index] = byte;
                let parsed = parse_target(&target);
                assert_eq!(
                    parsed.is_ok(),
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                );
                if let Ok(manifest) = parsed {
                    assert_eq!(encode_target(&manifest), target);
                }
            }
        }
    }
}
