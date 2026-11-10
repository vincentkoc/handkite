//! Bounded, read-only agreement between retained config, USB inventory and Lockdown.
//!
//! This is a prerequisite observation, not installation or selection authority.

#[cfg(target_os = "macos")]
use crate::usb::{PinnedUsb, Route};
use crate::{install_config::RetainedConfig, installer_lock::InstallerLock};
use handkite_core::protocol::DeviceBinding;
use std::future::Future;

const UNAVAILABLE: &str = "device agreement unavailable or changed";

/// Closed diagnostic categories. They carry no upstream error or device data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationFailure {
    UnsupportedPlatform,
    ConfigRetain,
    ConfigBefore,
    UsbBefore,
    Route,
    LockdownConnect,
    LockdownQuery(LockdownQueryFailure),
    Identity,
    UsbAfter,
    ConfigAfter,
    Deadline,
}

impl ObservationFailure {
    pub fn stage(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "observation_platform",
            Self::ConfigRetain => "config_retain",
            Self::ConfigBefore => "config_revalidate_before",
            Self::UsbBefore => "usb_inventory_before",
            Self::Route => "usb_route_agreement",
            Self::LockdownConnect => "lockdown_connect",
            Self::LockdownQuery(_) => "lockdown_query",
            Self::Identity => "lockdown_identity_agreement",
            Self::UsbAfter => "usb_inventory_after",
            Self::ConfigAfter => "config_revalidate_after",
            Self::Deadline => "observation_deadline",
        }
    }
}

/// Fixed query boundaries only; no socket, parser, or peer data is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockdownQueryFailure {
    RequestWrite,
    ResponseLength,
    ResponseRead,
    PlistDecode(PlistDecodeFailure),
    ReplyValidation,
}

impl LockdownQueryFailure {
    pub fn substage(self) -> &'static str {
        match self {
            Self::RequestWrite => "request_write",
            Self::ResponseLength => "response_length",
            Self::ResponseRead => "response_read",
            Self::PlistDecode(_) => "plist_decode",
            Self::ReplyValidation => "reply_validation",
        }
    }
}

/// Fixed classification of rejected reply decoding or plist structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlistDecodeFailure {
    BinaryFormat,
    OtherFormat,
    XmlPreflight,
    XmlPlistParse,
    XmlReplySchema(ReplyEnvelope),
}

impl PlistDecodeFailure {
    pub fn format(self) -> &'static str {
        match self {
            Self::BinaryFormat => "binary",
            Self::OtherFormat => "other",
            Self::XmlPreflight | Self::XmlPlistParse | Self::XmlReplySchema(_) => "xml",
        }
    }

    pub fn cause(self) -> &'static str {
        match self {
            Self::BinaryFormat => "binary_format",
            Self::OtherFormat => "non_xml_prefix",
            Self::XmlPreflight => "xml_preflight",
            Self::XmlPlistParse => "plist_syntax",
            Self::XmlReplySchema(_) => "reply_schema",
        }
    }
}

/// Presence classes only. Keys and values are never retained in this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyEnvelope {
    None,
    Error,
    Result,
    Other,
    ErrorResult,
    ErrorOther,
    ResultOther,
    ErrorResultOther,
    NotDictionary,
}

impl ReplyEnvelope {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Error => "error",
            Self::Result => "result",
            Self::Other => "other",
            Self::ErrorResult => "error_result",
            Self::ErrorOther => "error_other",
            Self::ResultOther => "result_other",
            Self::ErrorResultOther => "error_result_other",
            Self::NotDictionary => "not_dictionary",
        }
    }
}

/// Sealed observation owning config matched against an independent binding.
/// The config and observation borrow the same exact installer lock.
/// It exposes no identity, serializer, claim constructor, or install capability.
pub struct DeviceAgreement<'lock> {
    owner: &'lock InstallerLock,
    config: RetainedConfig<'lock>,
    #[cfg(target_os = "macos")]
    route: Route,
    poisoned: bool,
}

impl<'lock> DeviceAgreement<'lock> {
    /// Retain the lock's protected config against both independently supplied
    /// binding fields, then observe under one ten-second absolute deadline.
    /// The caller must obtain the expected binding independently of that config.
    /// This sends only mux ListDevices/Connect and one Lockdown GetValue for
    /// UniqueDeviceID.
    /// No pairing, session, service start, WDA, or persistent state is changed.
    pub async fn observe(
        owner: &'lock InstallerLock,
        expected: &DeviceBinding,
    ) -> Result<Self, &'static str> {
        Self::observe_diagnostic(owner, expected)
            .await
            .map_err(|_| UNAVAILABLE)
    }

    /// The same single observation, exposing only a fixed failure stage.
    pub async fn observe_diagnostic(
        owner: &'lock InstallerLock,
        expected: &DeviceBinding,
    ) -> Result<Self, ObservationFailure> {
        #[cfg(target_os = "macos")]
        {
            Self::observe_with(owner, expected, &mut FixedUsb).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (owner, expected);
            Err(ObservationFailure::UnsupportedPlatform)
        }
    }

    /// Repeat every observation and require the original UDID and mux device ID.
    /// Failure or cancellation permanently poisons this handle. Even dropping an
    /// unpolled reobservation future poisons it; obtain a fresh observation instead.
    pub fn reobserve(&mut self) -> impl Future<Output = Result<(), &'static str>> + '_ {
        #[cfg(target_os = "macos")]
        {
            self.reobserve_with(FixedUsb)
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.poisoned = true;
            let _ = (&self.owner, &self.config);
            std::future::ready(Err(UNAVAILABLE))
        }
    }

    /// Recheck the original config and exact lock without device transport.
    /// A poisoned observation is never reusable as retained selection evidence.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn revalidate_for_owner(
        &mut self,
        owner: &InstallerLock,
    ) -> Result<(), &'static str> {
        if self.poisoned || !std::ptr::eq(self.owner, owner) {
            return Err(UNAVAILABLE);
        }
        self.config.revalidate_for_owner(owner)
    }

    #[cfg(target_os = "macos")]
    async fn observe_with(
        owner: &'lock InstallerLock,
        expected: &DeviceBinding,
        transport: &mut impl Transport,
    ) -> Result<Self, ObservationFailure> {
        let config = RetainedConfig::acquire(owner, expected)
            .map_err(|_| ObservationFailure::ConfigRetain)?;
        Self::observe_retained_with(owner, config, transport).await
    }

    #[cfg(target_os = "macos")]
    async fn observe_retained_with(
        owner: &'lock InstallerLock,
        mut config: RetainedConfig<'lock>,
        transport: &mut impl Transport,
    ) -> Result<Self, ObservationFailure> {
        let deadline = tokio::time::Instant::now() + OBSERVATION_TIMEOUT;
        let route = observe_route(owner, &mut config, transport, deadline, None).await?;
        Ok(Self {
            owner,
            config,
            route,
            poisoned: false,
        })
    }

    #[cfg(target_os = "macos")]
    fn reobserve_with<'a>(
        &'a mut self,
        mut transport: impl Transport + 'a,
    ) -> impl Future<Output = Result<(), &'static str>> + 'a {
        let was_poisoned = std::mem::replace(&mut self.poisoned, true);
        let deadline = tokio::time::Instant::now() + OBSERVATION_TIMEOUT;
        async move {
            if was_poisoned {
                return Err(UNAVAILABLE);
            }
            observe_route(
                self.owner,
                &mut self.config,
                &mut transport,
                deadline,
                Some(&self.route),
            )
            .await
            .map_err(|_| UNAVAILABLE)?;
            self.poisoned = false;
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
const OBSERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(target_os = "macos")]
trait Transport {
    fn enumerate(
        &mut self,
        expected: &handkite_core::protocol::HardwareId,
    ) -> impl Future<Output = Result<Route, &'static str>>;
    fn unique_id(&mut self, route: &Route)
    -> impl Future<Output = Result<String, LockdownFailure>>;
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum LockdownFailure {
    Connect,
    Query(LockdownQueryFailure),
}

#[cfg(target_os = "macos")]
struct FixedUsb;

#[cfg(target_os = "macos")]
impl Transport for FixedUsb {
    async fn enumerate(
        &mut self,
        expected: &handkite_core::protocol::HardwareId,
    ) -> Result<Route, &'static str> {
        PinnedUsb::new(expected.clone())
            .route()
            .await
            .map_err(|_| UNAVAILABLE)
    }

    async fn unique_id(&mut self, route: &Route) -> Result<String, LockdownFailure> {
        let mut socket = route
            .lockdown_socket()
            .await
            .map_err(|_| LockdownFailure::Connect)?;
        lockdown::unique_id(&mut socket)
            .await
            .map_err(LockdownFailure::Query)
    }
}

#[cfg(target_os = "macos")]
async fn observe_route(
    owner: &InstallerLock,
    config: &mut RetainedConfig<'_>,
    transport: &mut impl Transport,
    deadline: tokio::time::Instant,
    original: Option<&Route>,
) -> Result<Route, ObservationFailure> {
    tokio::time::timeout_at(deadline, async {
        check_deadline(deadline)?;
        let validation = config.revalidate_for_owner(owner);
        check_deadline(deadline)?;
        validation.map_err(|_| ObservationFailure::ConfigBefore)?;
        let expected = config.binding().usb_id.clone();
        let route = transport.enumerate(&expected).await;
        check_deadline(deadline)?;
        let route = route.map_err(|_| ObservationFailure::UsbBefore)?;
        if route.udid() != expected.as_str() || original.is_some_and(|original| original != &route)
        {
            return Err(ObservationFailure::Route);
        }
        let reported = transport.unique_id(&route).await;
        check_deadline(deadline)?;
        let reported = reported.map_err(|failure| match failure {
            LockdownFailure::Connect => ObservationFailure::LockdownConnect,
            LockdownFailure::Query(query) => ObservationFailure::LockdownQuery(query),
        })?;
        if reported != route.udid() || reported != expected.as_str() {
            return Err(ObservationFailure::Identity);
        }
        let final_route = transport.enumerate(&expected).await;
        check_deadline(deadline)?;
        if final_route.map_err(|_| ObservationFailure::UsbAfter)? != route {
            return Err(ObservationFailure::UsbAfter);
        }
        let validation = config.revalidate_for_owner(owner);
        check_deadline(deadline)?;
        validation.map_err(|_| ObservationFailure::ConfigAfter)?;
        Ok(route)
    })
    .await
    .map_err(|_| ObservationFailure::Deadline)?
}

#[cfg(target_os = "macos")]
fn check_deadline(deadline: tokio::time::Instant) -> Result<(), ObservationFailure> {
    if tokio::time::Instant::now() >= deadline {
        Err(ObservationFailure::Deadline)
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "macos", test))]
mod lockdown {
    use super::{LockdownQueryFailure, PlistDecodeFailure, ReplyEnvelope};
    use crate::usb::plist_wire;
    use serde::{Deserialize, de::IgnoredAny};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    const MAX_REPLY_BYTES: usize = 4096;

    #[derive(Deserialize)]
    struct Reply {
        #[serde(rename = "Request")]
        request: plist::Value,
        #[serde(rename = "Value")]
        value: plist::Value,
        #[serde(rename = "Error")]
        error: Option<IgnoredAny>,
        #[serde(rename = "Result")]
        result: Option<plist::Value>,
    }

    pub(super) async fn unique_id(
        socket: &mut (impl AsyncRead + AsyncWrite + Unpin),
    ) -> Result<String, LockdownQueryFailure> {
        let mut request = plist::Dictionary::new();
        request.insert("Label".into(), "handkite".into());
        request.insert("Request".into(), "GetValue".into());
        request.insert("Key".into(), "UniqueDeviceID".into());
        let body = plist_wire::encode(request).map_err(|_| LockdownQueryFailure::RequestWrite)?;
        socket
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .map_err(|_| LockdownQueryFailure::RequestWrite)?;
        socket
            .write_all(&body)
            .await
            .map_err(|_| LockdownQueryFailure::RequestWrite)?;
        socket
            .flush()
            .await
            .map_err(|_| LockdownQueryFailure::RequestWrite)?;
        let length = socket
            .read_u32()
            .await
            .map_err(|_| LockdownQueryFailure::ResponseLength)? as usize;
        if length == 0 || length > MAX_REPLY_BYTES {
            return Err(LockdownQueryFailure::ResponseLength);
        }
        let mut body = vec![0; length];
        socket
            .read_exact(&mut body)
            .await
            .map_err(|_| LockdownQueryFailure::ResponseRead)?;
        let reply: Reply = plist_wire::decode(&body, MAX_REPLY_BYTES)
            .map_err(|_| LockdownQueryFailure::PlistDecode(classify_rejection(&body)))?;
        let value = reply
            .value
            .as_string()
            .filter(|value| !value.is_empty())
            .ok_or(LockdownQueryFailure::ReplyValidation)?;
        if reply.request.as_string() != Some("GetValue")
            || reply.error.is_some()
            || reply
                .result
                .as_ref()
                .is_some_and(|result| result.as_string() != Some("Success"))
        {
            return Err(LockdownQueryFailure::ReplyValidation);
        }
        // IgnoredAny can skip invalid dictionary keys. Validate the bounded
        // value tree, but never use it to replace the duplicate-aware Reply.
        plist::Value::from_reader_xml(body.as_slice())
            .map_err(|_| LockdownQueryFailure::PlistDecode(PlistDecodeFailure::XmlPlistParse))?;
        Ok(value.to_owned())
    }

    // Called only after direct Reply deserialization rejects. This can never
    // supply a Reply or turn a malformed/schema-invalid response into success.
    fn classify_rejection(body: &[u8]) -> PlistDecodeFailure {
        if body.starts_with(b"bplist00") {
            return PlistDecodeFailure::BinaryFormat;
        }
        if !body.starts_with(b"<") {
            return PlistDecodeFailure::OtherFormat;
        }
        if plist_wire::preflight(body, MAX_REPLY_BYTES).is_err() {
            return PlistDecodeFailure::XmlPreflight;
        }
        let value = match plist::Value::from_reader_xml(body) {
            Ok(value) => value,
            Err(_) => return PlistDecodeFailure::XmlPlistParse,
        };
        let envelope = match value.as_dictionary() {
            None => ReplyEnvelope::NotDictionary,
            Some(fields) => {
                let other = fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "Request" | "Value" | "Error" | "Result"));
                match (
                    fields.contains_key("Error"),
                    fields.contains_key("Result"),
                    other,
                ) {
                    (false, false, false) => ReplyEnvelope::None,
                    (true, false, false) => ReplyEnvelope::Error,
                    (false, true, false) => ReplyEnvelope::Result,
                    (false, false, true) => ReplyEnvelope::Other,
                    (true, true, false) => ReplyEnvelope::ErrorResult,
                    (true, false, true) => ReplyEnvelope::ErrorOther,
                    (false, true, true) => ReplyEnvelope::ResultOther,
                    (true, true, true) => ReplyEnvelope::ErrorResultOther,
                }
            }
        };
        PlistDecodeFailure::XmlReplySchema(envelope)
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod tests;
