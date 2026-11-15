use super::super::ClientInfo;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroizing;

const LOOKUP_CLIENT_INFO: &str =
    "<MacBookPro13,2> <macOS;13.1;22C65> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
const INVALID_CONTEXT: &str = "Apple provisioning context is invalid";

/// The same sixteen bytes must identify both Apple requests and the V3 state.
pub struct ProvisioningIdentifier(Zeroizing<[u8; 16]>);

impl ProvisioningIdentifier {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Header inputs from one validated sidecar response and one identifier.
pub struct AppleProvisioningContext {
    identifier: ProvisioningIdentifier,
    info: ClientInfo,
}

impl AppleProvisioningContext {
    pub fn new(identifier: ProvisioningIdentifier, info: ClientInfo) -> Self {
        Self { identifier, info }
    }

    pub fn identifier(&self) -> &ProvisioningIdentifier {
        &self.identifier
    }

    pub(super) fn headers(&self, lookup: bool) -> Result<HeaderMap, &'static str> {
        self.headers_at(lookup, OffsetDateTime::now_utc())
    }

    fn headers_at(&self, lookup: bool, now: OffsetDateTime) -> Result<HeaderMap, &'static str> {
        let bytes = self.identifier.as_bytes();
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let local_user = hex(digest.as_ref());
        let identifier = hex(bytes);
        let device_id = format!(
            "{}-{}-{}-{}-{}",
            &identifier[..8],
            &identifier[8..12],
            &identifier[12..16],
            &identifier[16..20],
            &identifier[20..]
        );
        let timestamp = now
            .to_offset(time::UtcOffset::UTC)
            .replace_nanosecond(0)
            .map_err(|_| INVALID_CONTEXT)?
            .format(&Rfc3339)
            .map_err(|_| INVALID_CONTEXT)?;
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (
                "x-mme-client-info",
                if lookup {
                    LOOKUP_CLIENT_INFO
                } else {
                    self.info.client_info()
                },
            ),
            ("x-apple-i-md-lu", local_user.as_str()),
            ("x-mme-device-id", device_id.as_str()),
            ("x-apple-i-client-time", timestamp.as_str()),
            ("x-apple-i-timezone", "UTC"),
            ("x-apple-locale", "en_US"),
        ] {
            let mut value = HeaderValue::from_str(value).map_err(|_| INVALID_CONTEXT)?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(self.info.user_agent()).map_err(|_| INVALID_CONTEXT)?,
        );
        Ok(headers)
    }
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

    #[test]
    fn headers_bind_identifier_and_use_utc_whole_seconds() {
        let context = AppleProvisioningContext::new(
            ProvisioningIdentifier::new([0xab; 16]),
            ClientInfo::parse(br#"{"client_info":"synthetic client","user_agent":"fixture/1"}"#)
                .unwrap(),
        );
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .replace_nanosecond(123)
            .unwrap();
        for lookup in [false, true] {
            let headers = context.headers_at(lookup, now).unwrap();
            assert_eq!(
                headers["x-apple-i-md-lu"],
                "5a2cfe8ab935918525d44fd6fd87c70fc83b4f29d1a727672e1b48f380473fc1"
            );
            assert_eq!(
                headers["x-mme-device-id"],
                "abababab-abab-abab-abab-abababababab"
            );
            assert_eq!(headers["x-apple-i-client-time"], "2023-11-14T22:13:20Z");
            assert_eq!(headers["x-apple-i-timezone"], "UTC");
            assert_eq!(headers["x-apple-locale"], "en_US");
            assert_eq!(headers[CONTENT_TYPE], "application/x-www-form-urlencoded");
            assert_eq!(headers[USER_AGENT], "fixture/1");
            assert_eq!(
                headers["x-mme-client-info"],
                if lookup {
                    LOOKUP_CLIENT_INFO
                } else {
                    "synthetic client"
                }
            );
        }
    }
}
