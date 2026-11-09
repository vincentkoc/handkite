use handkite_core::protocol::HardwareId;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use std::sync::Arc;
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbError {
    Unavailable,
    NotConnected,
    AmbiguousIdentity,
}

#[derive(Clone)]
pub struct PinnedUsb {
    expected: HardwareId,
    address: UsbmuxdAddr,
}

impl PinnedUsb {
    pub fn new(expected: HardwareId) -> Self {
        Self {
            expected,
            address: UsbmuxdAddr::UnixSocket("/var/run/usbmuxd".into()),
        }
    }

    pub async fn provider(&self) -> Result<Arc<dyn IdeviceProvider>, UsbError> {
        let mut mux = tokio::time::timeout(QUERY_TIMEOUT, self.address.connect(0))
            .await
            .map_err(|_| UsbError::Unavailable)?
            .map_err(|_| UsbError::Unavailable)?;
        let devices = tokio::time::timeout(QUERY_TIMEOUT, mux.get_devices())
            .await
            .map_err(|_| UsbError::Unavailable)?
            .map_err(|_| UsbError::Unavailable)?;
        let matching: Vec<_> = devices
            .into_iter()
            .filter(|device| {
                matches!(device.connection_type, Connection::Usb)
                    && device.udid == self.expected.as_str()
            })
            .collect();
        let device = match matching.as_slice() {
            [] => return Err(UsbError::NotConnected),
            [device] => device,
            _ => return Err(UsbError::AmbiguousIdentity),
        };
        let provider = UsbmuxdProvider {
            addr: self.address.clone(),
            tag: device.device_id,
            udid: device.udid.clone(),
            device_id: device.device_id,
            label: "handkite".into(),
        };
        Ok(Arc::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn production_route_ignores_ambient_mux_address() {
        const CHILD: &str = "HANDKITE_TEST_USB_ROUTE";
        if std::env::var_os(CHILD).is_some() {
            let usb = PinnedUsb::new(HardwareId::new("test-device").unwrap());
            assert_eq!(usb.expected.as_str(), "test-device");
            assert!(matches!(
                usb.address,
                UsbmuxdAddr::UnixSocket(path) if path == "/var/run/usbmuxd"
            ));
            return;
        }
        for address in ["127.0.0.1:9", "/tmp/test-usbmuxd", "invalid:address"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "usb::tests::production_route_ignores_ambient_mux_address",
                ])
                .env(CHILD, "1")
                .env("USBMUXD_SOCKET_ADDRESS", address)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
}
