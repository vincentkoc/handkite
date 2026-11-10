//! Apple provisioning formats and a callback gated by exact endpoint approval.

use reqwest::Url;
use zeroize::Zeroizing;

use super::{
    EndProvisioningData, MAX_CPIM_BYTES, ROUTING_INFO_HEADER, RoutingInfo, StartProvisioningData,
    validate_base64,
};

mod client;
mod context;
mod plist;
mod provider;
pub(super) use context::{AppleProvisioningContext, ProvisioningIdentifier};
pub(super) use provider::AppleProvisioningProvider;
#[cfg(test)]
pub(crate) use provider::tests::{
    fixture_empty_policy, fixture_lookup_only, fixture_provider, fixture_provider_for,
};

const MAX_LOOKUP_BYTES: usize = 64 * 1024;
const MAX_URL_BYTES: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = 128;
const INVALID_LOOKUP: &str = "Apple provisioning lookup is invalid";
const INVALID_RESPONSE: &str = "Apple provisioning response is invalid";
const INVALID_ROUTING: &str = "Apple provisioning routing information is invalid";

/// Builds the empty start request. This does not send an Apple request.
pub fn encode_start_request() -> Zeroizing<String> {
    Zeroizing::new("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Header</key><dict/><key>Request</key><dict/></dict></plist>".to_owned())
}

/// Accepts only canonical, nonempty Base64 containing at most 64 KiB of data.
pub fn encode_finish_request(cpim: &str) -> Result<Zeroizing<String>, &'static str> {
    validate_base64(cpim, MAX_CPIM_BYTES)?;
    // Canonical Base64 has no XML metacharacters.
    Ok(Zeroizing::new(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Header</key><dict/><key>Request</key><dict><key>cpim</key><string>{cpim}</string></dict></dict></plist>"
    )))
}

pub fn parse_start_response(
    http_status: u16,
    body: &[u8],
) -> Result<StartProvisioningData, &'static str> {
    let root = successful_response(http_status, body)?;
    StartProvisioningData::new(root.field("Response")?.field("spim")?.string()?.to_owned())
}

pub fn parse_finish_response(
    http_status: u16,
    body: &[u8],
) -> Result<EndProvisioningData, &'static str> {
    let root = successful_response(http_status, body)?;
    let response = root.field("Response")?;
    let ptm = response.field("ptm")?.string()?;
    let tk = response.field("tk")?.string()?;
    let routing_info = extract_routing_info(response.field(ROUTING_INFO_HEADER)?)?;
    EndProvisioningData::new(ptm.to_owned(), tk.to_owned(), routing_info)
}

fn extract_routing_info(value: &plist::Value) -> Result<RoutingInfo, &'static str> {
    match value {
        plist::Value::Integer(value) => {
            let value = u64::try_from(*value).map_err(|_| INVALID_ROUTING)?;
            if value == 0 {
                return Err(INVALID_ROUTING);
            }
            Ok(RoutingInfo(value))
        }
        plist::Value::String(value) if value.len() <= 20 => {
            RoutingInfo::parse_apple_header(value).map_err(|_| INVALID_ROUTING)
        }
        _ => Err(INVALID_ROUTING),
    }
}

fn successful_response(http_status: u16, body: &[u8]) -> Result<plist::Value, &'static str> {
    if http_status != 200 {
        return Err(INVALID_RESPONSE);
    }
    let root = plist::parse(body, MAX_RESPONSE_BYTES)?;
    if root
        .field("Response")?
        .field("Status")?
        .field("ec")?
        .integer()?
        != 0
    {
        return Err(INVALID_RESPONSE);
    }
    Ok(root)
}

/// Syntactically valid destinations supplied by untrusted lookup data.
/// This type is deliberately distinct from reviewed endpoints.
pub struct LookupCandidates {
    start: Url,
    finish: Url,
}

impl LookupCandidates {
    pub fn parse(http_status: u16, body: &[u8]) -> Result<Self, &'static str> {
        if http_status != 200 {
            return Err(INVALID_LOOKUP);
        }
        let root = plist::parse(body, MAX_LOOKUP_BYTES)?;
        let urls = root.field("urls")?;
        Ok(Self {
            start: candidate_url(urls.field("midStartProvisioning")?.string()?)?,
            finish: candidate_url(urls.field("midFinishProvisioning")?.string()?)?,
        })
    }

    pub fn start(&self) -> &str {
        self.start.as_str()
    }

    pub fn finish(&self) -> &str {
        self.finish.as_str()
    }
}

/// Exact reviewed URL pairs, never a hostname suffix or a caller-provided allowlist.
pub struct ReviewedEndpointPolicy {
    pairs: &'static [(&'static str, &'static str)],
}

impl ReviewedEndpointPolicy {
    /// Exact GrandSlam pair recorded in the protocol provenance in VALIDATION.md.
    pub fn production() -> Self {
        Self {
            pairs: &[(
                "https://gsa.apple.com/grandslam/MidService/startMachineProvisioning",
                "https://gsa.apple.com/grandslam/MidService/finishMachineProvisioning",
            )],
        }
    }

    pub fn approve(
        &self,
        candidates: &LookupCandidates,
    ) -> Result<ApprovedEndpoints, &'static str> {
        if !self
            .pairs
            .contains(&(candidates.start(), candidates.finish()))
        {
            return Err("Apple provisioning destinations are not approved");
        }
        Ok(ApprovedEndpoints {
            start: candidates.start.clone(),
            finish: candidates.finish.clone(),
        })
    }
}

/// Constructed only after both complete URLs match a reviewed pair.
pub struct ApprovedEndpoints {
    start: Url,
    finish: Url,
}

impl ApprovedEndpoints {
    pub fn start(&self) -> &str {
        self.start.as_str()
    }

    pub fn finish(&self) -> &str {
        self.finish.as_str()
    }
}

fn candidate_url(value: &str) -> Result<Url, &'static str> {
    if value.len() > MAX_URL_BYTES
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || byte == b'\\')
        || value.contains('#')
    {
        return Err(INVALID_LOOKUP);
    }
    // Reject encoded control characters and backslashes before URL normalization.
    for (index, character) in value.bytes().enumerate() {
        if character != b'%' {
            continue;
        }
        let escape = value.get(index + 1..index + 3).ok_or(INVALID_LOOKUP)?;
        let byte = u8::from_str_radix(escape, 16).map_err(|_| INVALID_LOOKUP)?;
        if byte <= b' ' || byte == 0x7f || byte == b'\\' {
            return Err(INVALID_LOOKUP);
        }
    }
    let authority = value
        .strip_prefix("https://")
        .ok_or(INVALID_LOOKUP)?
        .split(['/', '?'])
        .next()
        .ok_or(INVALID_LOOKUP)?;
    if authority.contains(['@', '%', '[', ']']) {
        return Err(INVALID_LOOKUP);
    }
    if let Some((_, port)) = authority.split_once(':')
        && port != "443"
    {
        return Err(INVALID_LOOKUP);
    }
    let url = Url::parse(value).map_err(|_| INVALID_LOOKUP)?;
    let host = url.host_str().ok_or(INVALID_LOOKUP)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.len() > 253
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(INVALID_LOOKUP);
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
