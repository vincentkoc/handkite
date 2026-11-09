use serde::Serialize;

pub const ROUTING_INFO_HEADER: &str = "X-Apple-I-MD-RINFO";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoutingInfo(u64);

impl RoutingInfo {
    pub fn parse_apple_header(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Apple routing information is invalid");
        }
        let value = value
            .parse::<u64>()
            .map_err(|_| "Apple routing information is invalid")?;
        if value == 0 {
            return Err("Apple routing information is zero");
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Serialize)]
pub struct FinishProvisioning<'a> {
    ptm: &'a str,
    tk: &'a str,
    #[serde(rename = "X-Apple-I-MD-RINFO")]
    routing_info: u64,
}

impl<'a> FinishProvisioning<'a> {
    pub fn new(ptm: &'a str, tk: &'a str, routing_info: RoutingInfo) -> Self {
        Self {
            ptm,
            tk,
            routing_info: routing_info.value(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_info_requires_apple_positive_decimal_header() {
        assert_eq!(
            RoutingInfo::parse_apple_header("17106176").unwrap().value(),
            17106176
        );
        for invalid in [
            "",
            "0",
            "-1",
            "1.0",
            " 1",
            "1 ",
            "1\n2",
            "18446744073709551616",
        ] {
            assert!(RoutingInfo::parse_apple_header(invalid).is_err());
        }
    }

    #[test]
    fn finish_message_forwards_apple_routing_info() {
        let routing_info = RoutingInfo::parse_apple_header("17106176").unwrap();
        let message = FinishProvisioning::new("ptm", "tk", routing_info);
        assert_eq!(
            serde_json::to_value(message).unwrap(),
            serde_json::json!({
                "ptm": "ptm",
                "tk": "tk",
                "X-Apple-I-MD-RINFO": 17106176
            })
        );
    }
}
