use zeroize::Zeroizing;

/// A one-shot secret. This type intentionally has no Debug or Serialize implementation.
pub struct Passcode(Zeroizing<String>);

impl Passcode {
    pub fn new(value: String) -> Result<Self, PasscodeError> {
        let value = Zeroizing::new(value);
        if !(4..=16).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PasscodeError::InvalidSecret);
        }
        Ok(Self(value))
    }

    pub fn len(&self) -> u32 {
        self.0.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasscodeProgress {
    pub total: u32,
    pub entered: u32,
}

impl PasscodeProgress {
    /// Interpret a secure-field label against the expected passcode length.
    /// The two numbers may be total-first or entered-first, in any language.
    pub fn parse(label: &str, expected_total: u32) -> Result<Self, PasscodeError> {
        let values = ascii_numbers(label).ok_or(PasscodeError::UnreadableProgress)?;
        let [a, b] = values;
        let total_first = a == expected_total && b <= expected_total;
        let entered_first = b == expected_total && a <= expected_total;
        if total_first {
            return Ok(Self {
                total: a,
                entered: b,
            });
        }
        if entered_first {
            return Ok(Self {
                total: b,
                entered: a,
            });
        }
        let observed = if (4..=16).contains(&a) && b <= a {
            Some(a)
        } else if (4..=16).contains(&b) && a <= b {
            Some(b)
        } else {
            None
        };
        match observed {
            Some(observed_total) => Err(PasscodeError::LengthMismatch { observed_total }),
            None => Err(PasscodeError::UnreadableProgress),
        }
    }
}

fn ascii_numbers(label: &str) -> Option<[u32; 2]> {
    let mut numbers = Vec::with_capacity(2);
    let mut current: Option<u32> = None;
    for byte in label.bytes().chain(std::iter::once(b' ')) {
        if byte.is_ascii_digit() {
            current = Some(
                current
                    .unwrap_or(0)
                    .checked_mul(10)?
                    .checked_add(u32::from(byte - b'0'))?,
            );
        } else if let Some(number) = current.take() {
            numbers.push(number);
            if numbers.len() > 2 {
                return None;
            }
        }
    }
    numbers.try_into().ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasscodeError {
    InvalidSecret,
    UnreadableProgress,
    LengthMismatch { observed_total: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_progress_orders_without_a_locale_rule() {
        for entered in 0..=6 {
            assert_eq!(
                PasscodeProgress::parse(&format!("6-digit passcode, {entered} entered"), 6),
                Ok(PasscodeProgress { total: 6, entered })
            );
            assert_eq!(
                PasscodeProgress::parse(&format!("{entered} of 6 digits"), 6),
                Ok(PasscodeProgress { total: 6, entered })
            );
        }
        assert_eq!(
            PasscodeProgress::parse(
                "\u{5171}6\u{4f4d}\u{6570}\u{ff0c}\u{5df2}\u{8f93}\u{5165}2\u{4f4d}",
                6
            ),
            Ok(PasscodeProgress {
                total: 6,
                entered: 2
            })
        );
    }

    #[test]
    fn refuses_length_mismatch_and_ambiguous_labels() {
        assert_eq!(
            PasscodeProgress::parse("0 of 4 digits", 6),
            Err(PasscodeError::LengthMismatch { observed_total: 4 })
        );
        assert_eq!(
            PasscodeProgress::parse("0 of 20 digits", 6),
            Err(PasscodeError::UnreadableProgress)
        );
        assert_eq!(
            PasscodeProgress::parse("3 of 6 digits, retry 1", 6),
            Err(PasscodeError::UnreadableProgress)
        );
    }

    #[test]
    fn accepts_only_a_numeric_one_shot_secret() {
        assert!(Passcode::new("1234".into()).is_ok());
        assert!(Passcode::new("123456".into()).is_ok());
        assert!(Passcode::new("12x456".into()).is_err());
        assert!(Passcode::new("123".into()).is_err());
    }
}
