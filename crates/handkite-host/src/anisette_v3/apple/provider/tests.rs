use super::super::client::tests::{FINISH, START, TlsFixture, context, response};
use super::*;
use std::time::Duration;
use tokio::task::JoinHandle;

fn document(fields: &str) -> Vec<u8> {
    format!("<plist version=\"1.0\"><dict>{fields}</dict></plist>").into_bytes()
}

fn successful(fields: &str) -> Vec<u8> {
    document(&format!(
        "<key>Response</key><dict><key>Status</key><dict><key>ec</key><integer>0</integer></dict>{fields}</dict>"
    ))
}

fn start_response() -> Vec<u8> {
    response("", &successful("<key>spim</key><string>YQ==</string>"))
}

fn finish_response(routing: &str) -> Vec<u8> {
    response(
        routing,
        &successful("<key>ptm</key><string>Yw==</string><key>tk</key><string>ZA==</string>"),
    )
}

fn provider(client: AppleClient) -> AppleProvisioningProvider {
    let policy = ReviewedEndpointPolicy {
        pairs: &[(START, FINISH)],
    };
    let candidates = LookupCandidates::parse(200, &lookup(START, FINISH)).unwrap();
    AppleProvisioningProvider {
        client,
        endpoints: policy.approve(&candidates).unwrap(),
        context: context(),
        stage: Stage::Fresh,
    }
}

fn lookup(start: &str, finish: &str) -> Vec<u8> {
    document(&format!(
        "<key>urls</key><dict><key>midStartProvisioning</key><string>{start}</string><key>midFinishProvisioning</key><string>{finish}</string></dict>"
    ))
}

pub(crate) async fn fixture_provider() -> (AppleProvisioningProvider, JoinHandle<Vec<Vec<u8>>>) {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(
            vec![
                response(
                    "Set-Cookie: synthetic=value; Secure\r\n",
                    &lookup(START, FINISH),
                ),
                start_response(),
                finish_response("X-Apple-I-MD-RINFO: 17106176\r\n"),
            ],
            false,
        )
        .await;
    let provider = AppleProvisioningProvider::discover_with(
        client,
        ReviewedEndpointPolicy {
            pairs: &[(START, FINISH)],
        },
        context(),
    )
    .await
    .unwrap();
    (provider, server)
}

#[tokio::test]
async fn approved_lookup_precedes_one_use_start_and_finish() {
    let (mut provider, server) = fixture_provider().await;
    assert_eq!(provider.identifier().as_bytes(), &[7; 16]);
    provider.start().await.unwrap();
    provider.finish("Yg==").await.unwrap();
    assert!(provider.finish("Yg==").await.is_err());
    assert!(provider.start().await.is_err());
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    for (request, path) in requests.iter().zip([
        "GET /grandslam/GsService2/lookup",
        "POST /start",
        "POST /finish",
    ]) {
        let request = std::str::from_utf8(request).unwrap();
        assert!(request.starts_with(&format!("{path} HTTP/1.1\r\n")));
        assert!(request.contains("user-agent: fixture/1\r\n"));
        assert!(request.contains("content-type: text/x-xml-plist\r\n"));
        assert!(request.contains("x-mme-device-id: 07070707-0707-0707-0707-070707070707\r\n"));
        assert!(!request.contains("accept-encoding:"));
        assert!(!request.contains("cookie:"));
        assert!(!request.contains("authorization:"));
    }
    assert!(
        std::str::from_utf8(&requests[0])
            .unwrap()
            .contains("<com.apple.AuthKit/1 (com.apple.akd/1.0)>")
    );
    assert!(
        std::str::from_utf8(&requests[1])
            .unwrap()
            .ends_with(&*encode_start_request())
    );
    assert!(
        std::str::from_utf8(&requests[2])
            .unwrap()
            .ends_with(&*encode_finish_request("Yg==").unwrap())
    );
}

#[tokio::test]
async fn empty_production_policy_and_unapproved_lookup_fail_before_provisioning() {
    assert_eq!(
        AppleProvisioningProvider::discover(context()).await.err(),
        Some("Apple provisioning destinations are not approved")
    );
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(
            vec![response(
                "",
                &lookup("https://unreviewed.example.test/start", FINISH),
            )],
            false,
        )
        .await;
    assert!(
        AppleProvisioningProvider::discover_with(
            client,
            ReviewedEndpointPolicy {
                pairs: &[(START, FINISH)]
            },
            context()
        )
        .await
        .is_err()
    );
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn finish_preserves_and_rejects_duplicate_routing_headers() {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(
            vec![
                start_response(),
                finish_response("X-Apple-I-MD-RINFO: 17106176\r\nx-apple-i-md-rinfo: 17106176\r\n"),
            ],
            false,
        )
        .await;
    let mut provider = provider(client);
    provider.start().await.unwrap();
    assert!(provider.finish("Yg==").await.is_err());
    assert!(provider.finish("Yg==").await.is_err());
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn callback_cancellation_consumes_start_or_finish_before_await() {
    for finish in [false, true] {
        let (client, server) = TlsFixture::new("provision.example.test", true, false)
            .serve(vec![start_response()], true)
            .await;
        let mut provider = provider(client);
        if finish {
            provider.stage = Stage::Started;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), provider.finish("Yg=="))
                    .await
                    .is_err()
            );
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), provider.start())
                    .await
                    .is_err()
            );
        }
        assert!(provider.start().await.is_err());
        assert!(provider.finish("Yg==").await.is_err());
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn dropped_unpolled_callback_and_invalid_order_are_consumed() {
    for finish in [false, true] {
        let mut provider = provider(AppleClient::new().unwrap());
        if finish {
            drop(provider.finish("Yg=="));
        } else {
            drop(provider.start());
        }
        assert!(provider.start().await.is_err());
        assert!(provider.finish("Yg==").await.is_err());
    }
}

#[tokio::test]
async fn malformed_start_and_invalid_cpim_cannot_retry() {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(vec![response("", b"synthetic-private-detail")], false)
        .await;
    let mut failed = provider(client);
    assert!(failed.start().await.is_err());
    assert!(failed.start().await.is_err());
    assert!(failed.finish("Yg==").await.is_err());
    assert_eq!(server.await.unwrap().len(), 1);
    let mut invalid = provider(AppleClient::new().unwrap());
    invalid.stage = Stage::Started;
    assert!(invalid.finish("invalid").await.is_err());
    assert!(invalid.finish("Yg==").await.is_err());
}
