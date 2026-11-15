use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub mod apple;
mod origin;
pub mod state;
mod tls;
mod transport;
pub use origin::AnisetteOrigin;
pub use tls::AnisetteIdentity;
pub use transport::{
    AnisetteClient, ClientInfo, EndProvisioningData, ProvisioningDataError,
    ProvisioningDataProvider, StartProvisioningData,
};

pub const ROUTING_INFO_HEADER: &str = "X-Apple-I-MD-RINFO";
const MAX_SERVER_MESSAGE_BYTES: usize = 4 * 1024 * 1024 + 4096;
const MAX_CPIM_BYTES: usize = 64 * 1024;
const MAX_ADI_STATE_BYTES: usize = 3 * 1024 * 1024;
const MAX_HEADERS_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(tag = "result")]
enum HeadersResponse {
    Headers {
        #[serde(rename = "X-Apple-I-MD")]
        one_time_password: String,
        #[serde(rename = "X-Apple-I-MD-M")]
        machine_id: String,
        #[serde(rename = "X-Apple-I-MD-RINFO")]
        routing_info: String,
    },
    GetHeadersError,
}

pub struct AppleSigningHeaders {
    one_time_password: String,
    machine_id: String,
    routing_info: RoutingInfo,
}

impl AppleSigningHeaders {
    pub fn parse(status: u16, body: &[u8]) -> Result<Self, &'static str> {
        if status != 200 || body.len() > MAX_HEADERS_RESPONSE_BYTES {
            return Err("Anisette header response failed");
        }
        let response: HeadersResponse =
            serde_json::from_slice(body).map_err(|_| "Anisette header response is invalid")?;
        match response {
            HeadersResponse::Headers {
                mut one_time_password,
                mut machine_id,
                routing_info,
            } => {
                let validation = if !valid_header_value(&one_time_password)
                    || !valid_header_value(&machine_id)
                {
                    Err("Anisette signing headers are invalid")
                } else {
                    RoutingInfo::parse_apple_header(&routing_info)
                };
                let routing_info = match validation {
                    Ok(value) => value,
                    Err(error) => {
                        one_time_password.zeroize();
                        machine_id.zeroize();
                        return Err(error);
                    }
                };
                Ok(Self {
                    one_time_password,
                    machine_id,
                    routing_info,
                })
            }
            HeadersResponse::GetHeadersError => Err("Anisette could not create signing headers"),
        }
    }

    pub fn one_time_password(&self) -> &str {
        &self.one_time_password
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn routing_info(&self) -> RoutingInfo {
        self.routing_info
    }
}

impl Drop for AppleSigningHeaders {
    fn drop(&mut self) {
        self.one_time_password.zeroize();
        self.machine_id.zeroize();
    }
}

fn valid_header_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

#[derive(Deserialize)]
struct StateEnvelope {
    provider: String,
    version: u32,
    binary_sha256: String,
    identifier: [u8; 16],
}

pub struct MacosProvisionedState {
    bytes: Vec<u8>,
    identifier: [u8; 16],
}

impl MacosProvisionedState {
    pub fn parse(mut bytes: Vec<u8>, identifier: [u8; 16]) -> Result<Self, &'static str> {
        let validation = || {
            if bytes.is_empty() || bytes.len() > MAX_ADI_STATE_BYTES {
                return Err("Anisette state size is invalid");
            }
            let envelope: StateEnvelope =
                serde_json::from_slice(&bytes).map_err(|_| "Anisette state is invalid")?;
            if envelope.provider != "macos-adid"
                || envelope.version != 1
                || envelope.identifier != identifier
                || envelope.binary_sha256.len() != 64
                || !envelope
                    .binary_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("Anisette state identity or backend mismatch");
            }
            Ok(())
        };
        if let Err(error) = validation() {
            bytes.zeroize();
            return Err(error);
        }
        Ok(Self { bytes, identifier })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn identifier(&self) -> &[u8; 16] {
        &self.identifier
    }
}

impl Drop for MacosProvisionedState {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisioningStage {
    Identifier,
    StartData,
    EndData,
    Result,
    Complete,
    Failed,
}

pub enum ProvisioningEvent {
    GiveIdentifier,
    GiveStartProvisioningData,
    GiveEndProvisioningData { cpim: String },
    ProvisioningSuccess { adi_pb: Vec<u8> },
}

#[derive(Deserialize)]
#[serde(tag = "result")]
enum ServerMessage {
    GiveIdentifier,
    GiveStartProvisioningData,
    GiveEndProvisioningData { cpim: String },
    ProvisioningSuccess { adi_pb: String },
    InvalidIdentifier,
    Timeout,
    StartProvisioningError,
    EndProvisioningError,
}

pub struct ProvisioningConversation {
    stage: ProvisioningStage,
}

impl Default for ProvisioningConversation {
    fn default() -> Self {
        Self {
            stage: ProvisioningStage::Identifier,
        }
    }
}

impl ProvisioningConversation {
    pub fn stage(&self) -> ProvisioningStage {
        self.stage
    }

    pub fn accept(&mut self, message: &[u8]) -> Result<ProvisioningEvent, &'static str> {
        let result = self.accept_inner(message);
        if result.is_err() {
            self.stage = ProvisioningStage::Failed;
        }
        result
    }

    fn accept_inner(&mut self, message: &[u8]) -> Result<ProvisioningEvent, &'static str> {
        if message.len() > MAX_SERVER_MESSAGE_BYTES {
            return Err("Anisette response is too large");
        }
        let value: serde_json::Value =
            serde_json::from_slice(message).map_err(|_| "Anisette response is invalid")?;
        let object = value.as_object().ok_or("Anisette response is invalid")?;
        let allowed = match object.get("result").and_then(serde_json::Value::as_str) {
            Some("GiveEndProvisioningData") => &["result", "cpim"][..],
            Some("ProvisioningSuccess") => &["result", "adi_pb"][..],
            Some("InvalidIdentifier" | "Timeout") => &["result"][..],
            Some("StartProvisioningError" | "EndProvisioningError") => &["result", "message"][..],
            Some("GiveIdentifier" | "GiveStartProvisioningData") => &["result"][..],
            _ => return Err("Anisette response is invalid"),
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("Anisette response is invalid");
        }
        let parsed: ServerMessage =
            serde_json::from_value(value).map_err(|_| "Anisette response is invalid")?;
        match (self.stage, parsed) {
            (ProvisioningStage::Identifier, ServerMessage::GiveIdentifier) => {
                self.stage = ProvisioningStage::StartData;
                Ok(ProvisioningEvent::GiveIdentifier)
            }
            (ProvisioningStage::StartData, ServerMessage::GiveStartProvisioningData) => {
                self.stage = ProvisioningStage::EndData;
                Ok(ProvisioningEvent::GiveStartProvisioningData)
            }
            (ProvisioningStage::EndData, ServerMessage::GiveEndProvisioningData { cpim }) => {
                validate_base64(&cpim, MAX_CPIM_BYTES)?;
                self.stage = ProvisioningStage::Result;
                Ok(ProvisioningEvent::GiveEndProvisioningData { cpim })
            }
            (ProvisioningStage::Result, ServerMessage::ProvisioningSuccess { adi_pb }) => {
                let state = decode_base64(&adi_pb, MAX_ADI_STATE_BYTES)?;
                self.stage = ProvisioningStage::Complete;
                Ok(ProvisioningEvent::ProvisioningSuccess { adi_pb: state })
            }
            (
                _,
                ServerMessage::InvalidIdentifier
                | ServerMessage::Timeout
                | ServerMessage::StartProvisioningError
                | ServerMessage::EndProvisioningError,
            ) => Err("Anisette provisioning failed"),
            _ => Err("Anisette provisioning response is out of order"),
        }
    }
}

fn validate_base64(value: &str, max_bytes: usize) -> Result<(), &'static str> {
    decode_base64(value, max_bytes).map(|_| ())
}

fn decode_base64(value: &str, max_bytes: usize) -> Result<Vec<u8>, &'static str> {
    if value.len() > max_bytes.div_ceil(3) * 4 {
        return Err("Anisette binary field is too large");
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| "Anisette binary field is invalid")?;
    if bytes.is_empty() || bytes.len() > max_bytes || STANDARD.encode(&bytes) != value {
        return Err("Anisette binary field is invalid");
    }
    Ok(bytes)
}

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

    #[test]
    fn provisioning_accepts_only_ordered_bounded_messages() {
        let mut conversation = ProvisioningConversation::default();
        assert!(matches!(
            conversation.accept(br#"{"result":"GiveIdentifier"}"#),
            Ok(ProvisioningEvent::GiveIdentifier)
        ));
        assert!(matches!(
            conversation.accept(br#"{"result":"GiveStartProvisioningData"}"#),
            Ok(ProvisioningEvent::GiveStartProvisioningData)
        ));
        assert!(matches!(
            conversation.accept(br#"{"result":"GiveEndProvisioningData","cpim":"YQ=="}"#),
            Ok(ProvisioningEvent::GiveEndProvisioningData { .. })
        ));
        assert!(matches!(
            conversation.accept(br#"{"result":"ProvisioningSuccess","adi_pb":"Yg=="}"#),
            Ok(ProvisioningEvent::ProvisioningSuccess { adi_pb }) if adi_pb == b"b"
        ));
        assert_eq!(conversation.stage(), ProvisioningStage::Complete);
    }

    #[test]
    fn malformed_or_early_response_disables_conversation() {
        for invalid in [
            br#"{"result":"GiveEndProvisioningData","cpim":"YQ=="}"#.as_slice(),
            br#"{"result":"InvalidIdentifier"}"#.as_slice(),
            br#"{"result":"GiveIdentifier","unexpected":true}"#.as_slice(),
        ] {
            let mut conversation = ProvisioningConversation::default();
            assert!(conversation.accept(invalid).is_err());
            assert_eq!(conversation.stage(), ProvisioningStage::Failed);
            assert!(
                conversation
                    .accept(br#"{"result":"GiveIdentifier"}"#)
                    .is_err()
            );
        }
    }

    #[test]
    fn provisioning_errors_discard_server_detail() {
        for result in ["StartProvisioningError", "EndProvisioningError"] {
            let mut conversation = ProvisioningConversation::default();
            let body = serde_json::to_vec(&serde_json::json!({
                "result": result, "message": "private server detail"
            }))
            .unwrap();
            assert!(matches!(
                conversation.accept(&body),
                Err("Anisette provisioning failed")
            ));
            assert_eq!(conversation.stage(), ProvisioningStage::Failed);
        }
    }

    #[test]
    fn macos_state_cannot_cross_backend_or_identifier() {
        let identifier = [7_u8; 16];
        let state = serde_json::json!({
            "provider": "macos-adid",
            "version": 1,
            "binary_sha256": "a".repeat(64),
            "identifier": identifier,
            "host_uuid": vec![8_u8; 16],
            "files": {}
        });
        let bytes = serde_json::to_vec(&state).unwrap();
        assert_eq!(
            MacosProvisionedState::parse(bytes.clone(), identifier)
                .unwrap()
                .as_bytes(),
            bytes
        );
        assert!(MacosProvisionedState::parse(bytes.clone(), [9_u8; 16]).is_err());
        let mut wrong_backend = state;
        wrong_backend["provider"] = "android".into();
        assert!(
            MacosProvisionedState::parse(serde_json::to_vec(&wrong_backend).unwrap(), identifier)
                .is_err()
        );
    }

    #[test]
    fn signing_headers_require_success_and_clean_values() {
        let body = br#"{"result":"Headers","X-Apple-I-MD":"otp","X-Apple-I-MD-M":"machine","X-Apple-I-MD-RINFO":"17106176"}"#;
        let headers = AppleSigningHeaders::parse(200, body).unwrap();
        assert_eq!(headers.one_time_password(), "otp");
        assert_eq!(headers.machine_id(), "machine");
        assert_eq!(headers.routing_info().value(), 17106176);
        assert!(AppleSigningHeaders::parse(503, body).is_err());
        assert!(
            AppleSigningHeaders::parse(
                200,
                br#"{"result":"GetHeadersError","message":"private detail"}"#
            )
            .is_err()
        );
        assert!(
            AppleSigningHeaders::parse(
                200,
                br#"{"result":"Headers","X-Apple-I-MD":"bad\nvalue","X-Apple-I-MD-M":"machine","X-Apple-I-MD-RINFO":"17106176"}"#
            )
            .is_err()
        );
    }
}
