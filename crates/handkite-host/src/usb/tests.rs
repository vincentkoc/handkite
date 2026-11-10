use super::*;
use std::process::Command;

fn device(connection: &str, udid: &str, device_id: u32) -> Device {
    Device {
        device_id,
        properties: Properties {
            connection: connection.into(),
            udid: udid.into(),
        },
    }
}

#[test]
fn only_one_exact_usb_match_is_selected_and_provider_keeps_actual_route() {
    let expected = HardwareId::new("test-device").unwrap();
    for devices in [
        vec![],
        vec![device("Network", "test-device", 9)],
        vec![device("USB", "TEST-device", 9)],
        vec![device("Other", "test-device", 9)],
    ] {
        assert_eq!(
            select(Inventory { devices }, &expected).err(),
            Some(UsbError::NotConnected)
        );
    }
    for second_id in [9, 10] {
        assert_eq!(
            select(
                Inventory {
                    devices: vec![
                        device("USB", "test-device", 9),
                        device("USB", "test-device", second_id)
                    ]
                },
                &expected
            )
            .err(),
            Some(UsbError::AmbiguousIdentity)
        );
    }
    let route = select(
        Inventory {
            devices: vec![
                device("Network", "test-device", 2),
                device("USB", "other", 3),
                device("USB", "test-device", 9),
            ],
        },
        &expected,
    )
    .unwrap();
    assert_eq!(route.udid, "test-device");
    assert_eq!(route.device_id, 9);
    let provider = route.provider();
    assert_eq!(provider.udid, route.udid);
    assert_eq!(provider.device_id, route.device_id);
    assert_eq!(provider.tag, route.device_id);
    assert!(matches!(provider.addr, UsbmuxdAddr::UnixSocket(path) if path == MUX_PATH));
}

#[test]
fn production_route_ignores_ambient_mux_address() {
    const CHILD: &str = "HANDKITE_TEST_USB_ROUTE";
    if std::env::var_os(CHILD).is_some() {
        let usb = PinnedUsb::new(HardwareId::new("test-device").unwrap());
        assert_eq!(usb.expected.as_str(), "test-device");
        assert!(
            matches!(fixed_address(), UsbmuxdAddr::UnixSocket(path) if path == "/var/run/usbmuxd")
        );
        return;
    }
    for address in ["127.0.0.1:9", "/tmp/test-usbmuxd", "invalid:address"] {
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "usb::tests::production_route_ignores_ambient_mux_address"
                ])
                .env(CHILD, "1")
                .env("USBMUXD_SOCKET_ADDRESS", address)
                .status()
                .unwrap()
                .success()
        );
    }
}
