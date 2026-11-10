use super::DeveloperCredential;
use crate::anisette_v3::{AppleSigningHeaders, ClientInfo, MacosProvisionedState};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const INVALID: &str = "developer request headers are invalid";

pub(super) fn headers(
    credential: &DeveloperCredential,
    state: &MacosProvisionedState,
    info: &ClientInfo,
    anisette: &AppleSigningHeaders,
) -> Result<HeaderMap, &'static str> {
    headers_at(credential, state, info, anisette, OffsetDateTime::now_utc())
}

fn headers_at(
    credential: &DeveloperCredential,
    state: &MacosProvisionedState,
    info: &ClientInfo,
    anisette: &AppleSigningHeaders,
    now: OffsetDateTime,
) -> Result<HeaderMap, &'static str> {
    let identifier = state.identifier();
    let digest = ring::digest::digest(&ring::digest::SHA256, identifier);
    let local_user = hex(digest.as_ref());
    let id = hex(identifier);
    let device_id = format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    );
    let time = now
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .map_err(|_| INVALID)?
        .format(&Rfc3339)
        .map_err(|_| INVALID)?;
    let routing = anisette.routing_info().value().to_string();
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-apple-i-identity-id", credential.adsid()),
        ("x-apple-gs-token", credential.token()),
        ("x-apple-i-md", anisette.one_time_password()),
        ("x-apple-i-md-m", anisette.machine_id()),
        ("x-apple-i-md-rinfo", routing.as_str()),
        ("x-apple-i-md-lu", local_user.as_str()),
        ("x-mme-device-id", device_id.as_str()),
        ("x-mme-client-info", info.client_info()),
        ("x-apple-i-client-time", time.as_str()),
        ("x-apple-i-timezone", "UTC"),
        ("x-apple-locale", "en_US"),
    ] {
        let mut value = HeaderValue::from_str(value).map_err(|_| INVALID)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/x-xml-plist"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/x-xml-plist"));
    headers.insert(USER_AGENT, HeaderValue::from_static("Xcode"));
    Ok(headers)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[(byte >> 4) as usize] as char,
                DIGITS[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anisette_v3::{ClientInfo, MacosProvisionedState};

    #[test]
    fn request_binds_one_identifier_and_marks_sensitive_headers() {
        let credential = DeveloperCredential::parse(b"12345\nsynthetic-token\n").unwrap();
        let state = MacosProvisionedState::parse(
            br#"{"provider":"macos-adid","version":1,"binary_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","identifier":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}"#.to_vec(),
            [7; 16],
        )
        .unwrap();
        let info =
            ClientInfo::parse(br#"{"client_info":"synthetic client","user_agent":"fixture/1"}"#)
                .unwrap();
        let anisette = AppleSigningHeaders::parse(
            200,
            br#"{"result":"Headers","X-Apple-I-MD":"otp","X-Apple-I-MD-M":"machine","X-Apple-I-MD-RINFO":"42"}"#,
        )
        .unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let headers = headers_at(&credential, &state, &info, &anisette, now).unwrap();
        assert_eq!(headers["x-apple-i-identity-id"], "12345");
        assert_eq!(headers["x-apple-gs-token"], "synthetic-token");
        assert_eq!(headers["x-apple-i-md-rinfo"], "42");
        assert_eq!(
            headers["x-mme-device-id"],
            "07070707-0707-0707-0707-070707070707"
        );
        assert_eq!(headers["x-apple-i-client-time"], "2023-11-14T22:13:20Z");
        for (name, _) in &headers {
            if ![CONTENT_TYPE, ACCEPT, USER_AGENT].contains(name) {
                assert!(headers[name].is_sensitive(), "{name}");
            }
        }
    }
}
