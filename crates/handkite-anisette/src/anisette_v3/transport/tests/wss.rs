use super::*;

type ServerSocket = WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

impl TlsFixture {
    async fn wss<F, Fut>(self, handler: F) -> (AnisetteClient, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(ServerSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let client = AnisetteClient {
            http: http_builder(&self.client)
                .resolve("anisette.example.invalid", address)
                .build()
                .unwrap(),
            tls: self.client.config,
            test_port: Some(address.port()),
        };
        let server = tokio::spawn(async move {
            timeout(Duration::from_secs(10), async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(tls) = TlsAcceptor::from(self.server).accept(tcp).await else {
                    return;
                };
                assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
                assert_eq!(tls.get_ref().1.peer_certificates().unwrap().len(), 1);
                let socket = tokio_tungstenite::accept_hdr_async(tls, check_upgrade)
                    .await
                    .unwrap();
                handler(socket).await;
                assert!(
                    timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "the client must not reconnect"
                );
            })
            .await
            .expect("bounded fixture");
        });
        (client, server)
    }
}

#[expect(
    clippy::result_large_err,
    reason = "tungstenite callback requires this error type"
)]
fn check_upgrade(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    assert_eq!(request.uri().path(), "/v3/provisioning_session");
    assert!(!request.headers().contains_key("origin"));
    Ok(response)
}

#[derive(Default)]
struct FixtureProvider {
    starts: usize,
    finishes: usize,
    stall: bool,
    fail: bool,
    start_delay: Duration,
}

impl ProvisioningDataProvider for FixtureProvider {
    async fn start(&mut self) -> Result<StartProvisioningData, ProvisioningDataError> {
        self.starts += 1;
        tokio::time::sleep(self.start_delay).await;
        if self.stall {
            std::future::pending::<()>().await;
        }
        if self.fail {
            return Err(ProvisioningDataError);
        }
        Ok(StartProvisioningData::new("YQ==".into()).unwrap())
    }

    async fn finish(&mut self, cpim: &str) -> Result<EndProvisioningData, ProvisioningDataError> {
        self.finishes += 1;
        assert_eq!(cpim, "Yg==");
        Ok(EndProvisioningData::new(
            "Yw==".into(),
            "ZA==".into(),
            RoutingInfo::parse_apple_header("17106176").unwrap(),
        )
        .unwrap())
    }
}

async fn challenge(socket: &mut ServerSocket, value: serde_json::Value) -> serde_json::Value {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
    serde_json::from_str(&message).unwrap()
}

async fn through_finish(socket: &mut ServerSocket) {
    assert_eq!(
        challenge(socket, serde_json::json!({"result":"GiveIdentifier"})).await,
        serde_json::json!({"identifier":STANDARD.encode([7; 16])})
    );
    assert_eq!(
        challenge(
            socket,
            serde_json::json!({"result":"GiveStartProvisioningData"})
        )
        .await,
        serde_json::json!({"spim":"YQ=="})
    );
    assert_eq!(
        challenge(
            socket,
            serde_json::json!({"result":"GiveEndProvisioningData","cpim":"Yg=="})
        )
        .await,
        serde_json::json!({"ptm":"Yw==","tk":"ZA==","X-Apple-I-MD-RINFO":17106176})
    );
}

#[tokio::test]
async fn wss_provisions_once_with_shared_mtls_and_routing_info() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            through_finish(&mut socket).await;
            socket.send(Message::Text(serde_json::json!({
            "result":"ProvisioningSuccess", "adi_pb":STANDARD.encode(state([7; 16]).as_bytes())
        }).to_string().into())).await.unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        })
        .await;
    let mut provider = FixtureProvider::default();
    let result = client.provision([7; 16], &mut provider).await.unwrap();
    assert_eq!(result.as_bytes(), state([7; 16]).as_bytes());
    assert_eq!((provider.starts, provider.finishes), (1, 1));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_composes_with_approved_apple_https_callbacks() {
    let (mut provider, apple_server) = crate::anisette_v3::apple::fixture_provider().await;
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            through_finish(&mut socket).await;
            socket.send(Message::Text(serde_json::json!({
                "result":"ProvisioningSuccess", "adi_pb":STANDARD.encode(state([7; 16]).as_bytes())
            }).to_string().into())).await.unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        })
        .await;
    let result = client
        .provision(*provider.identifier().as_bytes(), &mut provider)
        .await
        .unwrap();
    assert_eq!(result.as_bytes(), state([7; 16]).as_bytes());
    assert!(provider.start().await.is_err());
    assert!(provider.finish("Yg==").await.is_err());
    assert_eq!(apple_server.await.unwrap().len(), 3);
    server.await.unwrap();
}

#[tokio::test]
async fn wss_delayed_challenge_gets_a_fresh_reply_deadline() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            challenge(&mut socket, serde_json::json!({"result":"GiveIdentifier"})).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(
                challenge(
                    &mut socket,
                    serde_json::json!({"result":"GiveStartProvisioningData"})
                )
                .await,
                serde_json::json!({"spim":"YQ=="})
            );
            challenge(
                &mut socket,
                serde_json::json!({"result":"GiveEndProvisioningData","cpim":"Yg=="}),
            )
            .await;
            socket.send(Message::Text(serde_json::json!({
                "result":"ProvisioningSuccess", "adi_pb":STANDARD.encode(state([7; 16]).as_bytes())
            }).to_string().into())).await.unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        })
        .await;
    let mut provider = FixtureProvider {
        start_delay: Duration::from_secs(2),
        ..Default::default()
    };
    let result = client.provision([7; 16], &mut provider).await.unwrap();
    assert_eq!(result.as_bytes(), state([7; 16]).as_bytes());
    assert_eq!((provider.starts, provider.finishes), (1, 1));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_fresh_reply_budget_cannot_extend_the_conversation_deadline() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            challenge(&mut socket, serde_json::json!({"result":"GiveIdentifier"})).await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            socket
                .send(Message::Text(
                    r#"{"result":"GiveStartProvisioningData"}"#.into(),
                ))
                .await
                .unwrap();
            assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        })
        .await;
    let mut provider = FixtureProvider {
        start_delay: Duration::from_secs(2),
        ..Default::default()
    };
    let result = client
        .provision_until(
            [7; 16],
            &mut provider,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
    assert!(matches!(
        result,
        Err("Anisette provisioning stage timed out")
    ));
    assert_eq!((provider.starts, provider.finishes), (1, 0));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_rejects_untrusted_wrong_name_and_missing_client_identity() {
    for (name, trusted, authenticated) in [
        ("anisette.example.invalid", false, true),
        ("wrong.fixture.invalid", true, true),
        ("anisette.example.invalid", true, false),
    ] {
        let (client, server) = TlsFixture::new(name, trusted, authenticated)
            .wss(|_| async { panic!("TLS must reject before upgrade") })
            .await;
        let mut provider = FixtureProvider::default();
        assert!(matches!(
            client.provision([7; 16], &mut provider).await,
            Err("Anisette WSS handshake failed")
        ));
        assert_eq!((provider.starts, provider.finishes), (0, 0));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wss_rejects_redirect_without_reconnecting() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true).http(
        b"HTTP/1.1 302 Found\r\nLocation: https://fallback.invalid/\r\nContent-Length: 0\r\n\r\n".to_vec()
    ).await;
    let mut provider = FixtureProvider::default();
    assert!(client.provision([7; 16], &mut provider).await.is_err());
    assert_eq!((provider.starts, provider.finishes), (0, 0));
    assert!(!server.await.unwrap().is_empty());
}

#[tokio::test]
async fn wss_rejects_wrong_state_identity_after_finish() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            through_finish(&mut socket).await;
            socket.send(Message::Text(serde_json::json!({
            "result":"ProvisioningSuccess", "adi_pb":STANDARD.encode(state([8; 16]).as_bytes())
        }).to_string().into())).await.unwrap();
            let _ = socket.next().await;
        })
        .await;
    let mut provider = FixtureProvider::default();
    assert!(client.provision([7; 16], &mut provider).await.is_err());
    assert_eq!((provider.starts, provider.finishes), (1, 1));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_rejects_binary_out_of_order_malformed_closed_and_oversized_messages() {
    for message in [
        Message::Binary(b"private server data".to_vec().into()),
        Message::Text("not JSON".into()),
        Message::Text(r#"{"result":"GiveStartProvisioningData"}"#.into()),
        Message::Text("x".repeat(MAX_SERVER_MESSAGE_BYTES + 1).into()),
        Message::Close(None),
    ] {
        let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
            .wss(|mut socket| async move {
                let _ = socket.send(message).await;
                let _ = socket.next().await;
            })
            .await;
        let mut provider = FixtureProvider::default();
        let error = client
            .provision([7; 16], &mut provider)
            .await
            .err()
            .unwrap();
        assert!(!error.contains("private server data"));
        assert_eq!((provider.starts, provider.finishes), (0, 0));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wss_bounds_control_frames_without_advancing_conversation() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            for _ in 0..=MAX_CONTROL_FRAMES {
                let _ = socket.send(Message::Pong(Vec::new().into())).await;
            }
            let _ = socket.next().await;
        })
        .await;
    let mut provider = FixtureProvider::default();
    assert!(matches!(
        client.provision([7; 16], &mut provider).await,
        Err("Anisette provisioning control frame limit exceeded")
    ));
    assert_eq!((provider.starts, provider.finishes), (0, 0));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_control_frames_do_not_reset_read_deadline() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(|mut socket| async move {
            for _ in 0..MAX_CONTROL_FRAMES {
                tokio::time::sleep(Duration::from_millis(220)).await;
                if socket.send(Message::Pong(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        })
        .await;
    let mut provider = FixtureProvider::default();
    assert!(matches!(
        client.provision([7; 16], &mut provider).await,
        Err("Anisette provisioning stage timed out")
    ));
    assert_eq!((provider.starts, provider.finishes), (0, 0));
    server.await.unwrap();
}

#[tokio::test]
async fn wss_stops_on_provider_failure_or_deadline_without_replay() {
    for stall in [false, true] {
        let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
            .wss(|mut socket| async move {
                challenge(&mut socket, serde_json::json!({"result":"GiveIdentifier"})).await;
                socket
                    .send(Message::Text(
                        r#"{"result":"GiveStartProvisioningData"}"#.into(),
                    ))
                    .await
                    .unwrap();
                assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
            })
            .await;
        let mut provider = FixtureProvider {
            stall,
            fail: !stall,
            ..Default::default()
        };
        let error = client
            .provision([7; 16], &mut provider)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error,
            if stall {
                "Anisette provisioning stage timed out"
            } else {
                "Anisette provisioning data unavailable"
            }
        );
        assert_eq!((provider.starts, provider.finishes), (1, 0));
        server.await.unwrap();
    }
}

#[test]
fn outbound_fields_require_canonical_bounded_base64() {
    let routing = RoutingInfo::parse_apple_header("1").unwrap();
    for invalid in [
        String::new(),
        "YQ".into(),
        STANDARD.encode(vec![0; MAX_CPIM_BYTES + 1]),
    ] {
        assert!(StartProvisioningData::new(invalid.clone()).is_err());
        assert!(EndProvisioningData::new(invalid, "YQ==".into(), routing).is_err());
    }
}

#[cfg(target_os = "macos")]
mod reserved;
