use super::*;

async fn reply(body: &[u8], claimed: usize) -> Result<String, &'static str> {
    let (mut client, mut peer) = tokio::io::duplex(8192);
    let server = async {
        let length = peer.read_u32().await.unwrap() as usize;
        let mut request = vec![0; length];
        peer.read_exact(&mut request).await.unwrap();
        let request: plist::Dictionary = plist_wire::decode(&request, 4096).unwrap();
        assert_eq!(request.len(), 3);
        assert_eq!(request["Label"].as_string(), Some("handkite"));
        assert_eq!(request["Request"].as_string(), Some("GetValue"));
        assert_eq!(request["Key"].as_string(), Some("UniqueDeviceID"));
        peer.write_u32(claimed as u32).await.unwrap();
        peer.write_all(body).await.unwrap();
        peer.shutdown().await.unwrap();
    };
    let (result, ()) = tokio::join!(unique_id(&mut client), server);
    drop(client);
    let mut rest = Vec::new();
    peer.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    result
}

#[tokio::test]
async fn only_one_getvalue_is_sent_and_exact_string_is_preserved() {
    let body = b"<plist><dict><key>Request</key><string>GetValue</string><key>Value</key><string>TEST-device </string></dict></plist>";
    assert_eq!(reply(body, body.len()).await, Ok("TEST-device ".into()));
}

#[tokio::test]
async fn missing_nonstring_error_duplicate_and_malformed_replies_fail_closed() {
    for content in [
        "<key>Request</key><string>GetValue</string>",
        "<key>Value</key><string>test-device</string>",
        "<key>Request</key><string>Other</string><key>Value</key><string>test-device</string>",
        "<key>Request</key><string>GetValue</string><key>Value</key><integer>9</integer>",
        "<key>Request</key><string>GetValue</string><key>Value</key><string/>",
        "<key>Request</key><string>GetValue</string><key>Error</key><string>DeviceLocked</string><key>Value</key><string>test-device</string>",
        "<key>Request</key><string>GetValue</string><key>Value</key><string>other</string><key>Value</key><string>test-device</string>",
    ] {
        let body = format!("<plist><dict>{content}</dict></plist>");
        assert_eq!(reply(body.as_bytes(), body.len()).await, Err(UNAVAILABLE));
    }
    for (body, length) in [
        (b"bad".as_slice(), 3),
        (b"short", 7),
        (b"", 0),
        (b"", MAX_REPLY_BYTES + 1),
        (b"", u32::MAX as usize),
    ] {
        assert_eq!(reply(body, length).await, Err(UNAVAILABLE));
    }
}
