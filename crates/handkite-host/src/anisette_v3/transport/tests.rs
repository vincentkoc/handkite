use super::*;
use rcgen::{BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

mod wss;

struct TlsFixture {
    server: Arc<ServerConfig>,
    client: AnisetteIdentity,
}

fn ca() -> Certificate {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    Certificate::from_params(params).unwrap()
}

fn issued(
    ca: &Certificate,
    name: &str,
    usage: ExtendedKeyUsagePurpose,
) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    let mut params = CertificateParams::new(vec![name.into()]);
    params.extended_key_usages = vec![usage];
    let cert = Certificate::from_params(params).unwrap();
    (
        cert.serialize_der_with_signer(ca).unwrap().into(),
        cert.serialize_private_key_der().into(),
    )
}

impl TlsFixture {
    fn new(server_name: &str, trust_server: bool, authenticate_client: bool) -> Self {
        let authority = ca();
        let mut roots = RootCertStore::empty();
        roots
            .add(authority.serialize_der().unwrap().into())
            .unwrap();
        let (server_cert, server_key) =
            issued(&authority, server_name, ExtendedKeyUsagePurpose::ServerAuth);
        let (client_cert, client_key) = issued(
            &authority,
            "client.fixture.invalid",
            ExtendedKeyUsagePurpose::ClientAuth,
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots.clone()),
            provider.clone(),
        )
        .build()
        .unwrap();
        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![server_cert], server_key.into())
            .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(if trust_server {
                roots
            } else {
                RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
            });
        let mut client = if authenticate_client {
            builder
                .with_client_auth_cert(vec![client_cert], client_key.into())
                .unwrap()
        } else {
            builder.with_no_client_auth()
        };
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            server: Arc::new(server),
            client: AnisetteIdentity {
                config: Arc::new(client),
            },
        }
    }

    async fn http(self, response: Vec<u8>) -> (AnisetteClient, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let response = String::from_utf8(response)
            .unwrap()
            .replace(
                "https://fallback.invalid/",
                &format!(
                    "https://anisette.example.invalid:{}/redirected",
                    address.port()
                ),
            )
            .into_bytes();
        let client = AnisetteClient {
            http: http_builder(&self.client)
                .resolve("anisette.example.invalid", address)
                .build()
                .unwrap(),
            tls: self.client.config,
            test_port: Some(address.port()),
        };
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let Ok(mut stream) = TlsAcceptor::from(self.server).accept(tcp).await else {
                return Vec::new();
            };
            assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
            assert_eq!(stream.get_ref().1.peer_certificates().unwrap().len(), 1);
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = stream.read(&mut chunk).await.unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                assert!(request.len() <= 5 * 1024 * 1024);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() == end + 4 + length {
                        break;
                    }
                }
            }
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "the client must not redirect or retry"
            );
            request
        });
        (client, server)
    }
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn state(identifier: [u8; 16]) -> MacosProvisionedState {
    MacosProvisionedState::parse(
        serde_json::to_vec(&serde_json::json!({
            "provider": "macos-adid", "version": 1,
            "binary_sha256": "a".repeat(64), "identifier": identifier, "files": {}
        }))
        .unwrap(),
        identifier,
    )
    .unwrap()
}

#[tokio::test]
async fn https_client_info_requires_mutual_tls_and_fixed_path() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .http(response(
            "200 OK",
            br#"{"client_info":"fixture client","user_agent":"fixture/1"}"#,
        ))
        .await;
    let info = client.client_info().await.unwrap();
    assert_eq!(info.client_info(), "fixture client");
    assert_eq!(info.user_agent(), "fixture/1");
    let request = server.await.unwrap();
    assert!(request.starts_with(b"GET /v3/client_info HTTP/1.1\r\n"));
}

#[tokio::test]
async fn https_headers_send_only_bound_state_and_return_validated_headers() {
    let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
        .http(response("200 OK", br#"{"result":"Headers","X-Apple-I-MD":"otp","X-Apple-I-MD-M":"machine","X-Apple-I-MD-RINFO":"17106176"}"#))
        .await;
    let state = state([7; 16]);
    let headers = client.get_headers(&state).await.unwrap();
    assert_eq!(headers.routing_info().value(), 17106176);
    let request = server.await.unwrap();
    assert!(request.starts_with(b"POST /v3/get_headers HTTP/1.1\r\n"));
    let end = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap()
        + 4;
    let request: serde_json::Value = serde_json::from_slice(&request[end..]).unwrap();
    assert_eq!(
        request,
        serde_json::json!({
            "identifier": STANDARD.encode([7; 16]), "adi_pb": STANDARD.encode(state.as_bytes())
        })
    );
}

#[tokio::test]
async fn https_rejects_untrusted_wrong_name_and_missing_client_identity() {
    for (name, trusted, authenticated) in [
        ("anisette.example.invalid", false, true),
        ("wrong.fixture.invalid", true, true),
        ("anisette.example.invalid", true, false),
    ] {
        let (client, server) = TlsFixture::new(name, trusted, authenticated)
            .http(response("200 OK", b"{}"))
            .await;
        assert!(matches!(
            client.client_info().await,
            Err("Anisette HTTPS request failed")
        ));
        assert!(server.await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn https_rejects_redirects_errors_and_bounded_chunked_bodies() {
    let oversized = vec![b'x'; MAX_HEADERS_RESPONSE_BYTES + 1];
    let chunked = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
        oversized.len(), String::from_utf8(oversized.clone()).unwrap()
    ).into_bytes();
    for response in [
        b"HTTP/1.1 302 Found\r\nLocation: https://fallback.invalid/\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        response("503 Unavailable", b"private server detail"),
        response("200 OK", &oversized),
        chunked,
        response(
            "200 OK",
            br#"{"result":"GetHeadersError","message":"private server detail"}"#,
        ),
    ] {
        let (client, server) = TlsFixture::new("anisette.example.invalid", true, true)
            .http(response)
            .await;
        let error = client.get_headers(&state([7; 16])).await.err().unwrap();
        assert!(!error.contains("private server detail"));
        assert!(!server.await.unwrap().is_empty());
    }
}

#[test]
fn client_info_rejects_missing_extra_or_header_injection_fields() {
    for body in [
        br#"{}"#.as_slice(),
        br#"{"client_info":"ok","user_agent":"a\r\nb"}"#,
        br#"{"client_info":"ok","user_agent":"ok","extra":true}"#,
    ] {
        assert!(ClientInfo::parse(body).is_err());
    }
}
