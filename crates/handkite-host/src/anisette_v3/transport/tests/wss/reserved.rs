use super::*;
use crate::anisette_v3::state::tests::{fixture, is_reserved, load};
use std::fs;
use std::os::unix::fs::PermissionsExt;

const INFO: &[u8] = br#"{"client_info":"reserved client","user_agent":"reserved/1"}"#;

fn info_response(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

impl TlsFixture {
    async fn client_info_then_wss<F, Fut>(
        self,
        before_info_reply: impl FnOnce() + Send + 'static,
        expect_wss: bool,
        handler: F,
    ) -> (AnisetteClient, tokio::task::JoinHandle<()>)
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
            timeout(Duration::from_secs(10), async {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut stream = TlsAcceptor::from(self.server.clone())
                    .accept(tcp)
                    .await
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    request.push(stream.read_u8().await.unwrap());
                    assert!(request.len() <= 8192);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                assert!(request.starts_with(b"GET /v3/client_info HTTP/1.1\r\n"));
                before_info_reply();
                stream.write_all(&info_response(INFO)).await.unwrap();
                stream.shutdown().await.unwrap();
                if !expect_wss {
                    assert!(
                        timeout(Duration::from_millis(50), listener.accept())
                            .await
                            .is_err()
                    );
                    return;
                }
                let (tcp, _) = listener.accept().await.unwrap();
                let tls = TlsAcceptor::from(self.server).accept(tcp).await.unwrap();
                let socket = tokio_tungstenite::accept_hdr_async(tls, check_upgrade)
                    .await
                    .unwrap();
                handler(socket).await;
                assert!(
                    timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err()
                );
            })
            .await
            .expect("bounded fixture");
        });
        (client, server)
    }
}

async fn finish_for(socket: &mut ServerSocket, identifier: [u8; 16]) {
    assert_eq!(
        challenge(socket, serde_json::json!({"result":"GiveIdentifier"})).await,
        serde_json::json!({"identifier": STANDARD.encode(identifier)})
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

async fn send_success(socket: &mut ServerSocket, identifier: [u8; 16]) {
    socket.send(Message::Text(serde_json::json!({
        "result":"ProvisioningSuccess", "adi_pb": STANDARD.encode(state(identifier).as_bytes())
    }).to_string().into())).await.unwrap();
}

#[tokio::test]
async fn reserved_flow_fetches_one_info_and_binds_apple_wss_attempt_and_ready() {
    let (_scratch, layout, pending) = fixture();
    let identifier = *pending.identifier();
    let server_layout = layout.clone();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .client_info_then_wss(
            || {},
            true,
            move |mut socket| async move {
                finish_for(&mut socket, identifier).await;
                send_success(&mut socket, identifier).await;
                assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
                assert_eq!(
                    load(&server_layout).unwrap().as_bytes(),
                    state(identifier).as_bytes()
                );
            },
        )
        .await;
    let mut apple_server = None;
    let value = client
        .provision_apple_with_discovery(pending, |context| async {
            assert_eq!(context.identifier().as_bytes(), &identifier);
            let (provider, server) = crate::anisette_v3::apple::fixture_provider_for(context).await;
            apple_server = Some(server);
            Ok(provider)
        })
        .await
        .unwrap();
    assert_eq!(value.as_bytes(), load(&layout).unwrap().as_bytes());
    assert!(is_reserved(&layout));
    let requests = apple_server.unwrap().await.unwrap();
    assert_eq!(requests.len(), 3);
    let hex: String = identifier
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let device_id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    for (index, request) in requests.iter().enumerate() {
        let text = std::str::from_utf8(request).unwrap();
        assert!(text.contains(&format!("x-mme-device-id: {device_id}\r\n")));
        assert!(text.contains("user-agent: reserved/1\r\n"));
        if index > 0 {
            assert!(text.contains("x-mme-client-info: reserved client\r\n"));
        }
    }
    server.await.unwrap();
}

#[tokio::test]
async fn client_info_failure_consumes_reservation_before_discovery() {
    let (_scratch, layout, pending) = fixture();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .http(info_response(b"malformed"))
        .await;
    let result = client
        .provision_apple_with_discovery(pending, |_| async {
            panic!("discovery cannot run after invalid client info")
        })
        .await;
    assert!(result.is_err());
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
    assert!(!server.await.unwrap().is_empty());
}

#[tokio::test]
async fn empty_policy_after_info_consumes_reservation_without_wss() {
    let (_scratch, layout, pending) = fixture();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .http(info_response(INFO))
        .await;
    assert!(matches!(
        client
            .provision_apple_with_discovery(
                pending,
                crate::anisette_v3::apple::fixture_empty_policy
            )
            .await,
        Err("Apple provisioning destinations are not approved")
    ));
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
    assert!(!server.await.unwrap().is_empty());
}

#[tokio::test]
async fn unpolled_production_future_drop_preserves_reservation_without_requests() {
    let (_scratch, layout, pending) = fixture();
    let fixture = TlsFixture::new("anisette.example.invalid", true, true);
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let client = AnisetteClient {
        http: http_builder(&fixture.client)
            .resolve("anisette.example.invalid", address)
            .build()
            .unwrap(),
        tls: fixture.client.config,
        test_port: Some(address.port()),
    };
    drop(client.provision_apple(pending));
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
}

#[tokio::test]
async fn reserved_provider_failure_and_cancellation_do_not_replay() {
    for cancel in [false, true] {
        let (_scratch, layout, pending) = fixture();
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
            stall: cancel,
            fail: !cancel,
            ..Default::default()
        };
        let result = timeout(
            Duration::from_millis(500),
            client.provision_reserved(pending, &mut provider),
        )
        .await;
        if cancel {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        assert_eq!((provider.starts, provider.finishes), (1, 0));
        assert!(is_reserved(&layout));
        assert!(load(&layout).is_err());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wrong_success_identifier_preserves_pending_without_ready() {
    let (_scratch, layout, pending) = fixture();
    let identifier = *pending.identifier();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(move |mut socket| async move {
            finish_for(&mut socket, identifier).await;
            let mut wrong = identifier;
            wrong[0] ^= 1;
            send_success(&mut socket, wrong).await;
            let _ = socket.next().await;
        })
        .await;
    assert!(
        client
            .provision_reserved(pending, &mut FixtureProvider::default())
            .await
            .is_err()
    );
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn close_failure_and_close_cancellation_preserve_ready() {
    for cancel in [false, true] {
        let (_scratch, layout, pending) = fixture();
        let value = state(*pending.identifier());
        let future = persist_before_close(value, move |value| pending.persist(value), async {
            assert!(
                load(&layout).is_ok(),
                "ready must exist before the first close poll"
            );
            if cancel {
                std::future::pending::<()>().await;
            }
            Err("synthetic close failure")
        });
        let result = timeout(Duration::from_millis(20), future).await;
        if cancel {
            assert!(result.is_err());
        } else {
            assert!(matches!(result, Ok(Err("synthetic close failure"))));
        }
        assert!(load(&layout).is_ok());
        assert!(is_reserved(&layout));
    }
}

#[tokio::test]
async fn persistence_failure_never_polls_close_or_overwrites_residue() {
    let (_scratch, layout, pending) = fixture();
    let value = state(*pending.identifier());
    let stage = layout.anisette_state_directory().join("ready.stage");
    fs::write(&stage, b"existing").unwrap();
    assert!(
        persist_before_close(value, move |value| pending.persist(value), async {
            panic!("close must not be polled after failed persistence")
        })
        .await
        .is_err()
    );
    assert_eq!(fs::read(stage).unwrap(), b"existing");
    assert!(is_reserved(&layout));
    assert!(load(&layout).is_err());
}

fn change_attempt(layout: &crate::service_layout::ServiceLayout) {
    fs::set_permissions(
        layout.anisette_state_directory().join("attempt.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

#[tokio::test]
async fn changed_custody_rejects_before_client_info_connection() {
    let (_scratch, layout, pending) = fixture();
    change_attempt(&layout);
    let tls = TlsFixture::new("anisette.example.invalid", true, true);
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let client = AnisetteClient {
        http: http_builder(&tls.client)
            .resolve("anisette.example.invalid", address)
            .build()
            .unwrap(),
        tls: tls.client.config,
        test_port: Some(address.port()),
    };
    assert!(
        client
            .provision_apple_with_discovery(pending, |_| async {
                panic!("changed custody cannot reach discovery")
            })
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    assert!(is_reserved(&layout));
}

#[tokio::test]
async fn custody_changed_during_client_info_blocks_apple_discovery() {
    let (_scratch, layout, pending) = fixture();
    let mutate = layout.clone();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .client_info_then_wss(
            move || change_attempt(&mutate),
            false,
            |_| async { panic!("changed custody cannot reach WSS") },
        )
        .await;
    assert!(
        client
            .provision_apple_with_discovery(pending, |_| async {
                panic!("changed custody cannot reach discovery")
            })
            .await
            .is_err()
    );
    server.await.unwrap();
    assert!(is_reserved(&layout));
}

#[tokio::test]
async fn custody_changed_during_discovery_blocks_wss() {
    let (_scratch, layout, pending) = fixture();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .http(info_response(INFO))
        .await;
    let mut apple_server = None;
    assert!(matches!(
        client
            .provision_apple_with_discovery(pending, |context| async {
                let (provider, server) =
                    crate::anisette_v3::apple::fixture_lookup_only(context).await;
                apple_server = Some(server);
                change_attempt(&layout);
                Ok(provider)
            })
            .await,
        Err("Anisette state is unsafe, incomplete or changed")
    ));
    assert_eq!(apple_server.unwrap().await.unwrap().len(), 1);
    assert!(!server.await.unwrap().is_empty());
    assert!(is_reserved(&layout));
}

struct MutatingProvider {
    inner: FixtureProvider,
    layout: crate::service_layout::ServiceLayout,
    finish: bool,
}

impl ProvisioningDataProvider for MutatingProvider {
    async fn start(&mut self) -> Result<StartProvisioningData, ProvisioningDataError> {
        let data = self.inner.start().await?;
        if !self.finish {
            change_attempt(&self.layout);
        }
        Ok(data)
    }

    async fn finish(&mut self, cpim: &str) -> Result<EndProvisioningData, ProvisioningDataError> {
        let data = self.inner.finish(cpim).await?;
        if self.finish {
            change_attempt(&self.layout);
        }
        Ok(data)
    }
}

#[tokio::test]
async fn custody_changed_during_provider_await_blocks_reply() {
    for finish in [false, true] {
        let (_scratch, layout, pending) = fixture();
        let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
            .wss(move |mut socket| async move {
                challenge(&mut socket, serde_json::json!({"result":"GiveIdentifier"})).await;
                if finish {
                    challenge(
                        &mut socket,
                        serde_json::json!({"result":"GiveStartProvisioningData"}),
                    )
                    .await;
                }
                let message = if finish {
                    serde_json::json!({"result":"GiveEndProvisioningData","cpim":"Yg=="})
                } else {
                    serde_json::json!({"result":"GiveStartProvisioningData"})
                };
                socket
                    .send(Message::Text(message.to_string().into()))
                    .await
                    .unwrap();
                assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
            })
            .await;
        let mut provider = MutatingProvider {
            inner: FixtureProvider::default(),
            layout: layout.clone(),
            finish,
        };
        assert!(
            client
                .provision_reserved(pending, &mut provider)
                .await
                .is_err()
        );
        assert_eq!(
            (provider.inner.starts, provider.inner.finishes),
            (1, usize::from(finish))
        );
        assert!(is_reserved(&layout));
        assert!(load(&layout).is_err());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn custody_changed_before_challenge_blocks_identifier_reply() {
    let (_scratch, layout, pending) = fixture();
    let mutate = layout.clone();
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .wss(move |mut socket| async move {
            change_attempt(&mutate);
            socket
                .send(Message::Text(r#"{"result":"GiveIdentifier"}"#.into()))
                .await
                .unwrap();
            assert!(!matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        })
        .await;
    let mut provider = FixtureProvider::default();
    assert!(
        client
            .provision_reserved(pending, &mut provider)
            .await
            .is_err()
    );
    assert_eq!((provider.starts, provider.finishes), (0, 0));
    assert!(is_reserved(&layout));
    server.await.unwrap();
}
