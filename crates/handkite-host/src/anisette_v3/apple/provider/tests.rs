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
        "",
        &successful(&format!(
            "<key>ptm</key><string>Yw==</string><key>tk</key><string>ZA==</string>{routing}"
        )),
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
                finish_response("<key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>"),
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

pub(crate) async fn fixture_empty_policy(
    context: AppleProvisioningContext,
) -> Result<AppleProvisioningProvider, &'static str> {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(vec![], false)
        .await;
    let result = AppleProvisioningProvider::discover_with(
        client,
        ReviewedEndpointPolicy { pairs: &[] },
        context,
    )
    .await;
    assert!(server.await.unwrap().is_empty());
    result
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
        assert!(request.contains("content-type: application/x-www-form-urlencoded\r\n"));
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
    for (request, expected) in requests.iter().zip([
        "",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Header</key><dict/><key>Request</key><dict/></dict></plist>",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Header</key><dict/><key>Request</key><dict><key>cpim</key><string>Yg==</string></dict></dict></plist>",
    ]) {
        let (_, body) = std::str::from_utf8(request).unwrap().split_once("\r\n\r\n").unwrap();
        assert_eq!(body, expected);
    }
}

#[tokio::test]
async fn start_and_finish_close_keep_alive_connections_without_pooling() {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .keep_alive(vec![
            start_response(),
            finish_response("<key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>"),
        ])
        .await;
    let mut provider = provider(client);
    provider.start().await.unwrap();
    provider.finish("Yg==").await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with(b"POST /start HTTP/1.1\r\n"));
    assert!(requests[1].starts_with(b"POST /finish HTTP/1.1\r\n"));
}

#[tokio::test]
async fn empty_policy_and_unapproved_lookup_fail_before_provisioning() {
    assert_eq!(
        fixture_empty_policy(context()).await.err(),
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
async fn finish_rejects_duplicate_routing_plist_fields() {
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(
            vec![
                start_response(),
                finish_response("<key>X-Apple-I-MD-RINFO</key><integer>17106176</integer><key>X-Apple-I-MD-RINFO</key><integer>17106176</integer>"),
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
async fn finish_uses_only_plist_routing_information() {
    for routing in ["", "<key>X-Apple-I-MD-RINFO</key><string>17106176</string>"] {
        let (client, server) = TlsFixture::new("provision.example.test", true, false)
            .serve(
                vec![
                    start_response(),
                    response(
                        "X-Apple-I-MD-RINFO: 0\r\nx-apple-i-md-rinfo: 1\r\n",
                        &successful(&format!("<key>ptm</key><string>Yw==</string><key>tk</key><string>ZA==</string>{routing}")),
                    ),
                ],
                false,
            )
            .await;
        let mut provider = provider(client);
        provider.start().await.unwrap();
        assert_eq!(provider.finish("Yg==").await.is_ok(), !routing.is_empty());
        assert!(provider.finish("Yg==").await.is_err());
        assert_eq!(server.await.unwrap().len(), 2);
    }
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
        let (client, server) = TlsFixture::new("provision.example.test", true, false)
            .serve(vec![], false)
            .await;
        let mut provider = provider(client);
        if finish {
            drop(provider.finish("Yg=="));
        } else {
            drop(provider.start());
        }
        assert!(provider.start().await.is_err());
        assert!(provider.finish("Yg==").await.is_err());
        assert!(server.await.unwrap().is_empty());
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
    let (client, server) = TlsFixture::new("provision.example.test", true, false)
        .serve(vec![], false)
        .await;
    let mut invalid = provider(client);
    invalid.stage = Stage::Started;
    assert!(invalid.finish("invalid").await.is_err());
    assert!(invalid.finish("Yg==").await.is_err());
    assert!(server.await.unwrap().is_empty());
}
