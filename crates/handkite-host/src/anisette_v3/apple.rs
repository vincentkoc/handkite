//! Offline Apple provisioning formats. Parsing never approves a network destination.

use reqwest::Url;

mod plist;

const MAX_LOOKUP_BYTES: usize = 64 * 1024;
const MAX_URL_BYTES: usize = 4096;
const INVALID_LOOKUP: &str = "Apple provisioning lookup is invalid";

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
    /// No Apple provisioning destination has been reviewed for production yet.
    pub fn production() -> Self {
        Self { pairs: &[] }
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
    if let Some((_, port)) = authority.split_once(':') {
        if port != "443" {
            return Err(INVALID_LOOKUP);
        }
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
