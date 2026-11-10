use super::*;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::ReadBuf;

async fn reply(body: &[u8], claimed: usize) -> Result<String, LockdownQueryFailure> {
    let response = [&(claimed as u32).to_be_bytes()[..], body].concat();
    exchange(&response).await
}

async fn exchange(response: &[u8]) -> Result<String, LockdownQueryFailure> {
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
        peer.write_all(response).await.unwrap();
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
        "<key>Request</key><string>GetValue</string><key>Value</key><integer>9</integer>",
        "<key>Request</key><string>GetValue</string><key>Error</key><string>DeviceLocked</string><key>Value</key><string>test-device</string>",
        "<key>Request</key><string>GetValue</string><key>Value</key><string>other</string><key>Value</key><string>test-device</string>",
    ] {
        let body = format!("<plist><dict>{content}</dict></plist>");
        assert_eq!(
            reply(body.as_bytes(), body.len()).await,
            Err(LockdownQueryFailure::PlistDecode)
        );
    }
    for content in [
        "<key>Request</key><string>Other</string><key>Value</key><string>test-device</string>",
        "<key>Request</key><string>GetValue</string><key>Value</key><string/>",
    ] {
        let body = format!("<plist><dict>{content}</dict></plist>");
        assert_eq!(
            reply(body.as_bytes(), body.len()).await,
            Err(LockdownQueryFailure::ReplyValidation)
        );
    }
    assert_eq!(
        reply(b"bad", 3).await,
        Err(LockdownQueryFailure::PlistDecode)
    );
}

#[tokio::test]
async fn mixed_xml_content_in_lockdown_value_is_rejected() {
    for value in [
        "test-device<![CDATA[other]]>",
        "<![CDATA[other]]>test-device",
        "test-<![CDATA[other]]>device",
        "test-device&amp;",
        "test-device&#45;",
        "test-device&#x2D;",
        "test-&amp;device",
    ] {
        let body = format!(
            "<plist><dict><key>Request</key><string>GetValue</string><key>Value</key><string>{value}</string></dict></plist>"
        );
        assert_eq!(
            reply(body.as_bytes(), body.len()).await,
            Err(LockdownQueryFailure::PlistDecode)
        );
    }
}

#[tokio::test]
async fn response_length_rejects_truncated_headers_and_out_of_bounds_claims() {
    for prefix in [b"".as_slice(), &[0], &[0, 0], &[0, 0, 0]] {
        assert_eq!(
            exchange(prefix).await,
            Err(LockdownQueryFailure::ResponseLength)
        );
    }
    // The bounds category is shared with header reads; no body is supplied.
    for claimed in [0, MAX_REPLY_BYTES + 1, u32::MAX as usize] {
        assert_eq!(
            reply(b"", claimed).await,
            Err(LockdownQueryFailure::ResponseLength)
        );
    }
}

#[tokio::test]
async fn response_read_rejects_truncated_bodies_with_valid_length_claims() {
    for (body, claimed) in [(b"".as_slice(), 1), (b"short", 7), (b"", MAX_REPLY_BYTES)] {
        assert_eq!(
            reply(body, claimed).await,
            Err(LockdownQueryFailure::ResponseRead)
        );
    }
}

struct WriteFailure {
    fail_after: usize,
    fail_flush: bool,
    written: usize,
    failures: usize,
}

impl AsyncRead for WriteFailure {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        panic!("response read after failed request write");
    }
}

impl AsyncWrite for WriteFailure {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.written == self.fail_after {
            self.failures += 1;
            return Poll::Ready(Err(io::Error::other("private fixture write detail")));
        }
        let count = bytes.len().min(self.fail_after - self.written);
        self.written += count;
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail_flush {
            self.failures += 1;
            Poll::Ready(Err(io::Error::other("private fixture flush detail")))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        panic!("unexpected socket shutdown");
    }
}

#[tokio::test]
async fn request_write_covers_header_body_and_flush_without_reads_or_retries() {
    for fail_after in [0, 2, 4, 8, usize::MAX] {
        let mut socket = WriteFailure {
            fail_after,
            fail_flush: fail_after == usize::MAX,
            written: 0,
            failures: 0,
        };
        assert_eq!(
            unique_id(&mut socket).await,
            Err(LockdownQueryFailure::RequestWrite)
        );
        assert_eq!(socket.failures, 1);
        if socket.fail_flush {
            assert!(socket.written > 4);
        } else {
            assert_eq!(socket.written, fail_after);
        }
    }
}
