//! Advisory, read-only comparison with private pins for one exact process.

use serde::Deserialize;

pub const MAX_INPUT_BYTES: usize = 8192;
const MAX_PATH_BYTES: usize = 4096;

/// No variant grants selection, activation, termination, or handoff authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// Two complete samples agreed with all supplied pins.
    Match,
    /// Two complete, consistent samples differed from the supplied pins.
    Mismatch,
    /// Both initial PID lookups returned ESRCH. This is not restart exclusion.
    Absent,
    /// Unsupported platform, incomplete access, invalid data, or observed race.
    Unknown,
}

/// Caller-supplied identity, deliberately without Debug or output accessors.
/// Obtain these pins independently; parsing does not establish their provenance.
pub struct Expected {
    pid: i32,
    identity: Identity,
}

#[derive(PartialEq, Eq)]
struct Identity {
    start_seconds: u64,
    start_microseconds: u32,
    start_abstime: u64,
    effective_uid: u32,
    executable_path: String,
    executable_uuid: [u8; 16],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schema_version: u32,
    pid: i32,
    start_seconds: u64,
    start_microseconds: u32,
    start_abstime: u64,
    effective_uid: u32,
    executable_path: String,
    executable_uuid: String,
}

impl Expected {
    /// Parse one bounded versioned object, without path access or process queries.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_INPUT_BYTES
            || bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{')
        {
            return None;
        }
        let input: Input = serde_json::from_slice(bytes).ok()?;
        if input.schema_version != 1
            || input.pid <= 0
            || input.start_seconds == 0
            || input.start_microseconds >= 1_000_000
            || input.start_abstime == 0
            || !valid_path(&input.executable_path)
            || input.executable_uuid.len() != 32
        {
            return None;
        }
        let mut executable_uuid = [0; 16];
        for (output, pair) in executable_uuid
            .iter_mut()
            .zip(input.executable_uuid.as_bytes().chunks_exact(2))
        {
            let nibble = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            *output = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        if executable_uuid == [0; 16] {
            return None;
        }
        Some(Self {
            pid: input.pid,
            identity: Identity {
                start_seconds: input.start_seconds,
                start_microseconds: input.start_microseconds,
                start_abstime: input.start_abstime,
                effective_uid: input.effective_uid,
                executable_path: input.executable_path,
                executable_uuid,
            },
        })
    }
}

fn valid_path(path: &str) -> bool {
    path.len() < MAX_PATH_BYTES
        && path.starts_with('/')
        && !path.chars().any(char::is_control)
        && path[1..]
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

/// Compare only this PID using public macOS libproc APIs. No argv, environment,
/// process enumeration, filesystem executable lookup, signal, or subprocess.
///
/// Samples are point-in-time observations, not an atomic identity capability.
/// Same-value ABA changes and changes after the last read remain possible.
/// UUID/path identify executable metadata, not trusted code bytes or script argv.
/// No result proves a controller stopped or prevents it restarting elsewhere.
pub fn observe(expected: &Expected) -> Observation {
    #[cfg(target_os = "macos")]
    {
        compare(expected, macos::sample)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (expected.pid, &expected.identity);
        Observation::Unknown
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadFailure {
    Absent,
    Unknown,
}

#[cfg(any(target_os = "macos", test))]
fn compare(
    expected: &Expected,
    mut sample: impl FnMut(i32) -> Result<Identity, ReadFailure>,
) -> Observation {
    match sample(expected.pid) {
        Err(ReadFailure::Absent) => match sample(expected.pid) {
            Err(ReadFailure::Absent) => Observation::Absent,
            _ => Observation::Unknown,
        },
        Err(ReadFailure::Unknown) => Observation::Unknown,
        Ok(first) => match sample(expected.pid) {
            Ok(second) if first == second => {
                if first == expected.identity {
                    Observation::Match
                } else {
                    Observation::Mismatch
                }
            }
            _ => Observation::Unknown,
        },
    }
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(test)]
mod tests;
