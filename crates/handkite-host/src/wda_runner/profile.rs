//! Read installed provisioning-profile expiry for the runner before launch.
//!
//! Diagnosis only: an unexpired match never proves the runner can launch. It
//! exists so an expired free-account (7-day) profile is named before XCTest
//! starts, instead of surfacing as a generic initialization failure. Nothing
//! here logs or returns team identifiers, device identifiers or serials.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Per-profile CMS blob cap; real profiles are a few tens of KiB.
pub(super) const MAX_PROFILE_BYTES: usize = 1024 * 1024;
/// Bound on how many installed profiles are inspected.
const MAX_PROFILES: usize = 256;
/// Bound on embedded developer certificates per profile.
const MAX_CERTIFICATES: usize = 64;
/// Expiry closer than this is logged as a warning, never refused.
pub(super) const EXPIRING_WITHIN: Duration = Duration::from_secs(72 * 60 * 60);

/// Latest usable expiry among the profiles that cover the runner bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileExpiry {
    /// The profile's `ExpirationDate`.
    pub profile: SystemTime,
    /// Latest `notAfter` among the profile's developer certificates, if any parsed.
    pub certificate: Option<SystemTime>,
}

impl ProfileExpiry {
    /// The earlier of profile and certificate expiry: whichever ends first stops launch.
    pub fn effective(&self) -> SystemTime {
        self.certificate
            .map_or(self.profile, |certificate| certificate.min(self.profile))
    }
}

/// Whole hours from `now` until `at`, zero once passed.
pub fn hours_until(at: SystemTime, now: SystemTime) -> u64 {
    at.duration_since(now)
        .map_or(0, |left| left.as_secs() / 3600)
}

/// Choose the matching profile with the latest effective expiry.
pub(super) fn runner_profile_expiry(
    profiles: &[Vec<u8>],
    bundle_id: &str,
) -> Option<ProfileExpiry> {
    profiles
        .iter()
        .take(MAX_PROFILES)
        .filter_map(|blob| profile_for(blob, bundle_id))
        .max_by_key(ProfileExpiry::effective)
}

fn profile_for(blob: &[u8], bundle_id: &str) -> Option<ProfileExpiry> {
    let fields = embedded_plist(blob)?;
    let entitlements = fields.get("Entitlements")?.as_dictionary()?;
    let app_id = entitlements.get("application-identifier")?.as_string()?;
    let teams = fields.get("TeamIdentifier")?.as_array()?;
    if !teams
        .iter()
        .filter_map(plist::Value::as_string)
        .any(|team| covers(app_id, team, bundle_id))
    {
        return None;
    }
    let profile = SystemTime::from(fields.get("ExpirationDate")?.as_date()?);
    let certificate = fields
        .get("DeveloperCertificates")
        .and_then(plist::Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_CERTIFICATES)
        .filter_map(plist::Value::as_data)
        .filter_map(certificate_not_after)
        .max();
    Some(ProfileExpiry {
        profile,
        certificate,
    })
}

/// Exact `<TEAM>.<bundle>` or one terminal `.*` wildcard under the same team.
fn covers(app_id: &str, team: &str, bundle_id: &str) -> bool {
    if team.is_empty() || team.contains(['.', '*']) || bundle_id.contains('*') {
        return false;
    }
    let qualified = format!("{team}.{bundle_id}");
    if app_id == qualified {
        return true;
    }
    let Some(prefix) = app_id.strip_suffix('*') else {
        return false;
    };
    prefix.ends_with('.')
        && !prefix.contains('*')
        && prefix.starts_with(&format!("{team}."))
        && qualified.len() > prefix.len()
        && qualified.starts_with(prefix)
}

/// Extract the XML plist embedded in a profile's CMS envelope.
fn embedded_plist(blob: &[u8]) -> Option<plist::Dictionary> {
    if blob.len() > MAX_PROFILE_BYTES {
        return None;
    }
    const START: &[u8] = b"<?xml";
    const END: &[u8] = b"</plist>";
    let start = blob
        .windows(START.len())
        .position(|window| window == START)?;
    let end = blob[start..]
        .windows(END.len())
        .rposition(|window| window == END)?
        + start
        + END.len();
    plist::from_bytes::<plist::Dictionary>(&blob[start..end]).ok()
}

/// Minimal DER walk to `tbsCertificate.validity.notAfter`; no other field is read.
fn certificate_not_after(der: &[u8]) -> Option<SystemTime> {
    let (tag, certificate, _) = tlv(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = tlv(certificate)?;
    if tag != 0x30 {
        return None;
    }
    let mut rest = tbs;
    let (tag, _, after) = tlv(rest)?;
    if tag == 0xa0 {
        rest = after; // explicit version
    }
    for expected in [0x02, 0x30, 0x30] {
        // serialNumber, signature, issuer: skipped unread.
        let (tag, _, after) = tlv(rest)?;
        if tag != expected {
            return None;
        }
        rest = after;
    }
    let (tag, validity, _) = tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (_, _, after) = tlv(validity)?;
    let (tag, not_after, _) = tlv(after)?;
    der_time(tag, not_after)
}

fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, mut rest) = rest.split_first()?;
    let length = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, after) = rest.split_at(count);
        rest = after;
        bytes
            .iter()
            .fold(0usize, |length, byte| (length << 8) | usize::from(*byte))
    };
    if rest.len() < length {
        return None;
    }
    let (content, after) = rest.split_at(length);
    Some((tag, content, after))
}

fn der_time(tag: u8, value: &[u8]) -> Option<SystemTime> {
    let text = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<u32>().ok();
    let (year, offset) = match (tag, text.len()) {
        (0x17, 12) => {
            let short = number(0..2)?;
            (
                if short >= 50 {
                    1900 + short
                } else {
                    2000 + short
                },
                2,
            )
        }
        (0x18, 14) => (number(0..4)?, 4),
        _ => return None,
    };
    let month = number(offset..offset + 2)?;
    let day = number(offset + 2..offset + 4)?;
    let hour = number(offset + 4..offset + 6)?;
    let minute = number(offset + 6..offset + 8)?;
    let second = number(offset + 8..offset + 10)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    let seconds = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic placeholders only; never a real team or device identifier.
    const TEAM: &str = "TEAMFIXTUR";
    const RUNNER: &str = "com.example.HandkiteRunner.xctrunner";

    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if content.len() < 0x80 {
            out.push(content.len() as u8);
        } else {
            out.push(0x82);
            out.extend_from_slice(&(content.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(content);
        out
    }

    fn certificate(not_after: &[u8], tag: u8) -> Vec<u8> {
        let mut tbs = der(0xa0, &der(0x02, &[2]));
        tbs.extend(der(0x02, &[1, 2, 3]));
        tbs.extend(der(0x30, &der(0x06, &[0x2a])));
        tbs.extend(der(0x30, &der(0x31, &[0; 140])));
        let mut validity = der(0x17, b"260101000000Z");
        validity.extend(der(tag, not_after));
        tbs.extend(der(0x30, &validity));
        tbs.extend(der(0x30, &[]));
        let mut certificate = der(0x30, &tbs);
        certificate.extend(der(0x30, &der(0x06, &[0x2a])));
        der(0x30, &certificate)
    }

    fn profile(app_id: &str, expires: SystemTime, certificates: Vec<Vec<u8>>) -> Vec<u8> {
        let mut entitlements = plist::Dictionary::new();
        entitlements.insert("application-identifier".into(), app_id.into());
        let mut fields = plist::Dictionary::new();
        fields.insert(
            "TeamIdentifier".into(),
            vec![plist::Value::from(TEAM)].into(),
        );
        fields.insert("ExpirationDate".into(), plist::Date::from(expires).into());
        fields.insert("Entitlements".into(), entitlements.into());
        fields.insert(
            "DeveloperCertificates".into(),
            certificates
                .into_iter()
                .map(plist::Value::Data)
                .collect::<Vec<_>>()
                .into(),
        );
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, &fields).unwrap();
        // Fake CMS framing around the XML, like a real `.mobileprovision`.
        let mut blob = vec![0x30, 0x80, 0x06, 0x09];
        blob.extend(xml);
        blob.extend([0xa0, 0x82, 0x01, 0x00]);
        blob
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn der_times_decode_utc_and_generalized_forms() {
        // 2026-11-16T12:00:00Z
        let expected = at(1_794_830_400);
        assert_eq!(
            certificate_not_after(&certificate(b"261116120000Z", 0x17)),
            Some(expected)
        );
        assert_eq!(
            certificate_not_after(&certificate(b"20261116120000Z", 0x18)),
            Some(expected)
        );
        assert_eq!(der_time(0x17, b"491231235959Z"), Some(at(2_524_607_999)));
        assert_eq!(der_time(0x17, b"500101000000Z"), None); // 1950 is pre-epoch
        assert_eq!(der_time(0x17, b"261316120000Z"), None);
        assert_eq!(der_time(0x17, b"2611161200Z"), None);
        assert_eq!(certificate_not_after(b"\x30\x05garbage"), None);
        assert_eq!(certificate_not_after(&[]), None);
    }

    #[test]
    fn matches_exact_and_terminal_wildcard_only() {
        assert!(covers(&format!("{TEAM}.{RUNNER}"), TEAM, RUNNER));
        assert!(covers(&format!("{TEAM}.*"), TEAM, RUNNER));
        assert!(covers(&format!("{TEAM}.com.example.*"), TEAM, RUNNER));
        assert!(!covers("OTHERTEAM0.*", TEAM, RUNNER));
        assert!(!covers(&format!("{TEAM}.com.other.*"), TEAM, RUNNER));
        assert!(!covers(&format!("{TEAM}.com.*.xctrunner"), TEAM, RUNNER));
        assert!(!covers(&format!("{TEAM}*"), TEAM, RUNNER));
        assert!(!covers(&format!("{TEAM}.{RUNNER}.extra"), TEAM, RUNNER));
        assert!(!covers("*", TEAM, RUNNER));
    }

    #[test]
    fn chooses_latest_matching_profile_and_bounds_input() {
        let early = at(1_800_000_000);
        let late = at(1_900_000_000);
        let unrelated = at(2_000_000_000);
        let profiles = vec![
            profile(&format!("{TEAM}.{RUNNER}"), early, vec![]),
            profile(
                &format!("{TEAM}.*"),
                late,
                vec![certificate(b"280101000000Z", 0x17)],
            ),
            profile(&format!("{TEAM}.com.other.app"), unrelated, vec![]),
            b"not a profile".to_vec(),
            vec![b'<'; MAX_PROFILE_BYTES + 1],
        ];
        let chosen = runner_profile_expiry(&profiles, RUNNER).unwrap();
        assert_eq!(chosen.profile, late);
        // 2028-01-01T00:00:00Z
        assert_eq!(chosen.certificate, Some(at(1_830_297_600)));
        assert_eq!(chosen.effective(), at(1_830_297_600));
        assert_eq!(runner_profile_expiry(&profiles[2..], RUNNER), None);
    }

    #[test]
    fn certificate_expiry_bounds_effective_expiry_and_hours_floor() {
        let expiry = ProfileExpiry {
            profile: at(10 * 3600),
            certificate: Some(at(5 * 3600 + 59)),
        };
        assert_eq!(expiry.effective(), at(5 * 3600 + 59));
        assert_eq!(hours_until(expiry.effective(), at(0)), 5);
        assert_eq!(hours_until(at(0), at(1)), 0);
    }
}
