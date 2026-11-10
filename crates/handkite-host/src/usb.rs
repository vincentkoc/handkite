use handkite_core::protocol::HardwareId;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use std::sync::Arc;
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const MUX_PATH: &str = "/var/run/usbmuxd";

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
    fn provider(&self) -> UsbmuxdProvider {
        UsbmuxdProvider {
            addr: fixed_address(),
            tag: self.device_id,
            udid: self.udid.clone(),
            device_id: self.device_id,
            label: "handkite".into(),
        }
    }
}

fn fixed_address() -> UsbmuxdAddr {
    UsbmuxdAddr::UnixSocket(MUX_PATH.into())
}

impl PinnedUsb {
    pub fn new(expected: HardwareId) -> Self {
        Self { expected }
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
}

struct Inventory {
    devices: Vec<Device>,
}
struct Device {
    device_id: u32,
    properties: Properties,
}
struct Properties {
    connection: String,
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

#[cfg(test)]
mod tests;
