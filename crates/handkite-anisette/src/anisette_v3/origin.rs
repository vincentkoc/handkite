//! Operator-supplied Anisette sidecar origin. There is deliberately no default.

use std::fmt;

const INVALID: &str = "Anisette origin must be https://HOST or https://HOST:PORT with a lowercase DNS hostname and no path, query, or credentials";
const DEFAULT_PORT: u16 = 443;

/// One HTTPS origin for the V3 sidecar: a lowercase DNS hostname and a port.
///
/// IP literals, user information, paths other than `/`, queries, fragments,
/// and non-HTTPS schemes are rejected. The canonical form omits port 443 and
/// is what the Mac-side attempt record binds to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnisetteOrigin {
    host: String,
    port: u16,
}

impl AnisetteOrigin {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let rest = value.strip_prefix("https://").ok_or(INVALID)?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty()
            || authority
                .bytes()
                .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b'@' | b'[' | b']'))
        {
            return Err(INVALID);
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, parse_port(port)?),
            None => (authority, DEFAULT_PORT),
        };
        if !valid_host(host) {
            return Err(INVALID);
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host` or `host:port`, omitting the default HTTPS port.
    pub(crate) fn authority(&self) -> String {
        if self.port == DEFAULT_PORT {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for AnisetteOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "https://{}", self.authority())
    }
}

fn parse_port(value: &str) -> Result<u16, &'static str> {
    if value.is_empty()
        || value.len() > 5
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(INVALID);
    }
    value.parse::<u16>().map_err(|_| INVALID)
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    // A numeric final label would make an IPv4 literal; require a DNS name.
    labels.iter().all(label_ok)
        && !labels
            .last()
            .is_some_and(|label| label.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_dns_origins_and_canonicalizes_default_port() {
        let origin = AnisetteOrigin::parse("https://anisette.example.invalid").unwrap();
        assert_eq!(
            (origin.host(), origin.port()),
            ("anisette.example.invalid", 443)
        );
        assert_eq!(origin.to_string(), "https://anisette.example.invalid");
        for spelling in [
            "https://anisette.example.invalid/",
            "https://anisette.example.invalid:443",
            "https://anisette.example.invalid:443/",
        ] {
            assert_eq!(AnisetteOrigin::parse(spelling).unwrap(), origin);
        }
        let custom = AnisetteOrigin::parse("https://a-1.example.invalid:8443").unwrap();
        assert_eq!(custom.port(), 8443);
        assert_eq!(custom.to_string(), "https://a-1.example.invalid:8443");
    }

    #[test]
    fn rejects_everything_but_a_bare_https_dns_origin() {
        for invalid in [
            "",
            "anisette.example.invalid",
            "http://anisette.example.invalid",
            "wss://anisette.example.invalid",
            "HTTPS://anisette.example.invalid",
            "https://",
            "https:///",
            "https://Anisette.example.invalid",
            "https://localhost",
            "https://127.0.0.1",
            "https://[::1]",
            "https://user@anisette.example.invalid",
            "https://anisette.example.invalid/v3",
            "https://anisette.example.invalid//",
            "https://anisette.example.invalid?x=1",
            "https://anisette.example.invalid#x",
            "https://anisette.example.invalid:",
            "https://anisette.example.invalid:0",
            "https://anisette.example.invalid:0443",
            "https://anisette.example.invalid:65536",
            "https://anisette.example.invalid:+1",
            "https://anisette..invalid",
            "https://-anisette.example.invalid",
            "https://anisette-.example.invalid",
            "https://anisette_v3.example.invalid",
            "https://anisette.example.invalid.",
            " https://anisette.example.invalid",
            "https://anisette.example.invalid ",
        ] {
            assert_eq!(AnisetteOrigin::parse(invalid), Err(INVALID), "{invalid:?}");
        }
        let long_label = format!("https://{}.invalid", "a".repeat(64));
        assert!(AnisetteOrigin::parse(&long_label).is_err());
    }
}
