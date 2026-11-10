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

async fn request(socket: &mut tokio::io::DuplexStream) -> plist::Dictionary {
    let mut header = [0; 16];
    socket.read_exact(&mut header).await.unwrap();
    assert_eq!(&header[4..], [1u32, 8, TAG].map(u32::to_le_bytes).concat());
    let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize - 16;
    assert!(length <= MAX_MUX_BYTES);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    plist_wire::decode(&body, MAX_MUX_BYTES).unwrap()
}

#[tokio::test]
async fn enumeration_sends_only_listdevices_and_returns_actual_route() {
    let (mut client, mut peer) = tokio::io::duplex(8192);
    let server = async {
        let request = request(&mut peer).await;
        assert_eq!(request.len(), 3);
        assert_eq!(request["MessageType"].as_string(), Some("ListDevices"));
        assert_eq!(request["ClientVersionString"].as_string(), Some("handkite"));
        assert_eq!(request["kLibUSBMuxVersion"].as_unsigned_integer(), Some(3));
        let body = b"<plist><dict><key>DeviceList</key><array><dict><key>DeviceID</key><integer>37</integer><key>Properties</key><dict><key>ConnectionType</key><string>USB</string><key>SerialNumber</key><string>test-device</string></dict></dict></array></dict></plist>";
        for word in [(body.len() + 16) as u32, 1, 8, TAG] {
            peer.write_u32_le(word).await.unwrap();
        }
        peer.write_all(body).await.unwrap();
    };
    let expected = HardwareId::new("test-device").unwrap();
    let (result, ()) = tokio::join!(enumerate(&mut client, &expected), server);
    let route = result.unwrap();
    assert_eq!(route.udid, "test-device");
    assert_eq!(route.device_id, 37);
    drop(client);
    let mut rest = Vec::new();
    peer.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn connect_transcript_contains_only_exact_route_and_lockdown_port() {
    let (mut client, mut peer) = tokio::io::duplex(8192);
    let response = async {
        let request = request(&mut peer).await;
        assert_eq!(request.len(), 3);
        assert_eq!(request["MessageType"].as_string(), Some("Connect"));
        assert_eq!(request["DeviceID"].as_unsigned_integer(), Some(37));
        assert_eq!(
            request["PortNumber"].as_unsigned_integer(),
            Some(62078u16.to_be() as u64)
        );
        let mut reply = plist::Dictionary::new();
        reply.insert("MessageType".into(), "Result".into());
        reply.insert("Number".into(), 0.into());
        let body = plist_wire::encode(reply).unwrap();
        for word in [(body.len() + 16) as u32, 1, 8, TAG] {
            peer.write_u32_le(word).await.unwrap();
        }
        peer.write_all(&body).await.unwrap();
    };
    let (result, ()) = tokio::join!(connect_lockdown(&mut client, 37), response);
    assert_eq!(result, Ok(()));
    drop(client);
    let mut rest = Vec::new();
    peer.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn malformed_mux_headers_fail_before_reading_or_allocating_body() {
    for header in [
        [0, 1, 8, TAG],
        [16, 1, 8, TAG],
        [u32::MAX, 1, 8, TAG],
        [20, 0, 8, TAG],
        [20, 1, 0, TAG],
        [20, 1, 8, TAG + 1],
    ] {
        let (mut client, mut peer) = tokio::io::duplex(8192);
        let response = async {
            request(&mut peer).await;
            for word in header {
                peer.write_u32_le(word).await.unwrap();
            }
        };
        let (result, ()) = tokio::join!(connect_lockdown(&mut client, 9), response);
        assert_eq!(result, Err(UsbError::Unavailable));
    }
}

#[test]
fn bounded_xml_rejects_duplicate_fields_binary_dtd_depth_and_extra_roots() {
    for body in [
        "<plist><dict><key>DeviceList</key><array/><key>DeviceList</key><array/></dict></plist>"
            .to_string(),
        "bplist00".to_string(),
        "<!DOCTYPE plist [<!ENTITY x 'expanded'>]><plist><dict/></plist>".to_string(),
        format!(
            "<plist>{}{}{}</plist>",
            "<array>".repeat(17),
            "<string>x</string>",
            "</array>".repeat(17)
        ),
        "<plist><dict/></plist><plist><dict/></plist>".to_string(),
    ] {
        assert!(plist_wire::decode::<Inventory>(body.as_bytes(), MAX_MUX_BYTES).is_err());
    }
    let body = b"<plist><dict><key>DeviceList</key><array/></dict></plist>";
    assert!(plist_wire::decode::<Inventory>(body, body.len() - 1).is_err());
    assert!(plist_wire::decode::<Inventory>(body, body.len()).is_ok());
    let deep = format!(
        "<plist>{}{}{}</plist>",
        "<array>".repeat(17),
        "<string>x</string>",
        "</array>".repeat(17)
    );
    assert!(plist_wire::decode::<plist::Value>(deep.as_bytes(), MAX_MUX_BYTES).is_err());
    let many = format!("<plist><array>{}</array></plist>", "<string/>".repeat(4096));
    assert!(plist_wire::decode::<plist::Value>(many.as_bytes(), MAX_MUX_BYTES).is_err());
    for suffix in ["trailing", "&amp;", "<?after root?>"] {
        let extra = [body.as_slice(), suffix.as_bytes()].concat();
        assert!(plist_wire::decode::<Inventory>(&extra, MAX_MUX_BYTES).is_err());
    }
    let mut binary = Vec::new();
    plist::Value::String("test-device".into())
        .to_writer_binary(&mut binary)
        .unwrap();
    assert!(plist_wire::decode::<plist::Value>(&binary, MAX_MUX_BYTES).is_err());
}
