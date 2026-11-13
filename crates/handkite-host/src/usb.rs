use handkite_core::protocol::HardwareId;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
#[cfg(any(target_os = "macos", test))]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(any(target_os = "macos", test))]
pub(crate) mod plist_wire;

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const MUX_PATH: &str = "/var/run/usbmuxd";
#[cfg(any(target_os = "macos", test))]
const MAX_MUX_BYTES: usize = 64 * 1024;
#[cfg(any(target_os = "macos", test))]
const TAG: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbError {
    Unavailable,
    NotConnected,
    AmbiguousIdentity,
}

#[derive(Clone)]
pub struct PinnedUsb {
    expected: HardwareId,
}

/// Actual enumeration result. Never reconstructed from the configured identity.
#[derive(PartialEq, Eq)]
pub(crate) struct Route {
    udid: String,
    device_id: u32,
}

impl Route {
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn fixture(udid: &str, device_id: u32) -> Self {
        Self {
            udid: udid.into(),
            device_id,
        }
    }

    fn provider(&self) -> UsbmuxdProvider {
        UsbmuxdProvider {
            addr: fixed_address(),
            tag: self.device_id,
            udid: self.udid.clone(),
            device_id: self.device_id,
            label: "handkite".into(),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn udid(&self) -> &str {
        &self.udid
    }

    #[cfg(target_os = "macos")]
    pub(crate) async fn lockdown_socket(&self) -> Result<idevice::IdeviceSocket, UsbError> {
        let mut socket = fixed_address()
            .to_socket()
            .await
            .map_err(|_| UsbError::Unavailable)?;
        connect_lockdown(&mut socket, self.device_id).await?;
        // Bypass upstream plist reads and tracing; keep only the connected socket.
        idevice::Idevice::new(socket, "handkite")
            .get_socket()
            .ok_or(UsbError::Unavailable)
    }
}

fn fixed_address() -> UsbmuxdAddr {
    UsbmuxdAddr::UnixSocket(MUX_PATH.into())
}

impl PinnedUsb {
    pub fn new(expected: HardwareId) -> Self {
        Self { expected }
    }

    pub(crate) fn expected_id(&self) -> &HardwareId {
        &self.expected
    }

    pub async fn provider(&self) -> Result<Arc<dyn IdeviceProvider>, UsbError> {
        // Preserve the existing live provider's pinned-library transport behavior.
        let mut mux = tokio::time::timeout(QUERY_TIMEOUT, fixed_address().connect(0))
            .await
            .map_err(|_| UsbError::Unavailable)?
            .map_err(|_| UsbError::Unavailable)?;
        let devices = tokio::time::timeout(QUERY_TIMEOUT, mux.get_devices())
            .await
            .map_err(|_| UsbError::Unavailable)?
            .map_err(|_| UsbError::Unavailable)?;
        let inventory = Inventory {
            devices: devices
                .into_iter()
                .map(|device| Device {
                    device_id: device.device_id,
                    properties: Properties {
                        connection: if matches!(device.connection_type, Connection::Usb) {
                            "USB"
                        } else {
                            ""
                        }
                        .into(),
                        udid: device.udid,
                    },
                })
                .collect(),
        };
        Ok(Arc::new(select(inventory, &self.expected)?.provider()))
    }

    #[cfg(target_os = "macos")]
    pub(crate) async fn route(&self) -> Result<Route, UsbError> {
        tokio::time::timeout(QUERY_TIMEOUT, async {
            let mut socket = fixed_address()
                .to_socket()
                .await
                .map_err(|_| UsbError::Unavailable)?;
            enumerate(&mut socket, &self.expected).await
        })
        .await
        .map_err(|_| UsbError::Unavailable)?
    }
}

#[cfg(any(target_os = "macos", test))]
async fn enumerate(
    socket: &mut (impl AsyncRead + AsyncWrite + Unpin),
    expected: &HardwareId,
) -> Result<Route, UsbError> {
    let mut request = plist::Dictionary::new();
    request.insert("MessageType".into(), "ListDevices".into());
    request.insert("ClientVersionString".into(), "handkite".into());
    request.insert("kLibUSBMuxVersion".into(), 3.into());
    let response: Inventory = exchange(socket, request).await?;
    select(response, expected)
}

#[derive(Deserialize)]
struct Inventory {
    #[serde(rename = "DeviceList")]
    devices: Vec<Device>,
}

#[derive(Deserialize)]
struct Device {
    #[serde(rename = "DeviceID")]
    device_id: u32,
    #[serde(rename = "Properties")]
    properties: Properties,
}

#[derive(Deserialize)]
struct Properties {
    #[serde(rename = "ConnectionType")]
    connection: String,
    #[serde(rename = "SerialNumber")]
    udid: String,
}

fn select(inventory: Inventory, expected: &HardwareId) -> Result<Route, UsbError> {
    let mut matches = inventory.devices.into_iter().filter(|device| {
        device.properties.connection == "USB" && device.properties.udid == expected.as_str()
    });
    let device = matches.next().ok_or(UsbError::NotConnected)?;
    if matches.next().is_some() {
        return Err(UsbError::AmbiguousIdentity);
    }
    Ok(Route {
        udid: device.properties.udid,
        device_id: device.device_id,
    })
}

#[cfg(any(target_os = "macos", test))]
async fn connect_lockdown(
    socket: &mut (impl AsyncRead + AsyncWrite + Unpin),
    device_id: u32,
) -> Result<(), UsbError> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(rename = "MessageType")]
        message_type: String,
        #[serde(rename = "Number")]
        number: u32,
    }
    let mut request = plist::Dictionary::new();
    request.insert("MessageType".into(), "Connect".into());
    request.insert("DeviceID".into(), device_id.into());
    request.insert("PortNumber".into(), 62078u16.to_be().into());
    let response: Reply = exchange(socket, request).await?;
    if response.message_type != "Result" || response.number != 0 {
        return Err(UsbError::Unavailable);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
async fn exchange<T: serde::de::DeserializeOwned>(
    socket: &mut (impl AsyncRead + AsyncWrite + Unpin),
    request: plist::Dictionary,
) -> Result<T, UsbError> {
    let body = plist_wire::encode(request).map_err(|_| UsbError::Unavailable)?;
    for word in [(body.len() + 16) as u32, 1, 8, TAG] {
        socket
            .write_all(&word.to_le_bytes())
            .await
            .map_err(|_| UsbError::Unavailable)?;
    }
    socket
        .write_all(&body)
        .await
        .map_err(|_| UsbError::Unavailable)?;
    socket.flush().await.map_err(|_| UsbError::Unavailable)?;
    let mut header = [0u8; 16];
    socket
        .read_exact(&mut header)
        .await
        .map_err(|_| UsbError::Unavailable)?;
    let words: Vec<_> = header
        .chunks_exact(4)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    let length = words[0].checked_sub(16).ok_or(UsbError::Unavailable)? as usize;
    if words[1..] != [1, 8, TAG] || length == 0 || length > MAX_MUX_BYTES {
        return Err(UsbError::Unavailable);
    }
    let mut body = vec![0; length];
    socket
        .read_exact(&mut body)
        .await
        .map_err(|_| UsbError::Unavailable)?;
    plist_wire::decode(&body, MAX_MUX_BYTES).map_err(|_| UsbError::Unavailable)
}

#[cfg(test)]
mod tests;
