use super::super::{super::ClientInfo, ProvisioningIdentifier};
use super::*;
use rcgen::{BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

pub(in crate::anisette_v3::apple) struct TlsFixture {
    server: Arc<ServerConfig>,
    roots: RootCertStore,
}

pub(in crate::anisette_v3::apple) const START: &str = "https://provision.example.test/start";
pub(in crate::anisette_v3::apple) const FINISH: &str = "https://provision.example.test/finish";

pub(in crate::anisette_v3::apple) fn context() -> AppleProvisioningContext {
    AppleProvisioningContext::new(
        ProvisioningIdentifier::new([7; 16]),
        ClientInfo::parse(br#"{"client_info":"synthetic client","user_agent":"fixture/1"}"#)
            .unwrap(),
    )
}

pub(in crate::anisette_v3::apple) fn response(headers: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

impl TlsFixture {
    pub(in crate::anisette_v3::apple) fn new(
        name: &str,
        trusted: bool,
        require_client: bool,
    ) -> Self {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = Certificate::from_params(ca_params).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.serialize_der().unwrap().into()).unwrap();
        let mut cert_params = CertificateParams::new(vec![name.into(), "gsa.apple.com".into()]);
        cert_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = Certificate::from_params(cert_params).unwrap();
        let key: PrivatePkcs8KeyDer<'static> = cert.serialize_private_key_der().into();
        let cert: CertificateDer<'static> = cert.serialize_der_with_signer(&ca).unwrap().into();
        let crypto = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots.clone()),
            crypto.clone(),
        )
        .allow_unauthenticated()
        .build()
        .unwrap();
        let builder = ServerConfig::builder_with_provider(crypto.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let mut server = if require_client {
            builder.with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots.clone()),
                    crypto,
                )
                .build()
                .unwrap(),
            )
        } else {
            builder.with_client_cert_verifier(verifier)
        }
        .with_single_cert(vec![cert], key.into())
        .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        if !trusted {
            roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        Self {
            server: Arc::new(server),
            roots,
        }
    }

    pub(in crate::anisette_v3::apple) async fn serve(
        self,
        responses: Vec<Vec<u8>>,
        stall: bool,
    ) -> (AppleClient, JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let client = AppleClient {
            http: builder(self.roots)
                .unwrap()
                .resolve("provision.example.test", address)
                .resolve("gsa.apple.com", address)
                .build()
                .unwrap(),
            test_port: Some(address.port()),
        };
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut requests = Vec::new();
                for response in responses {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let Ok(mut stream) = TlsAcceptor::from(self.server.clone()).accept(tcp).await
                    else {
                        return requests;
                    };
                    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
                    assert!(stream.get_ref().1.peer_certificates().is_none());
                    let mut request = Vec::new();
                    let mut chunk = [0; 4096];
                    loop {
                        let count = stream.read(&mut chunk).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..count]);
                        assert!(request.len() <= 256 * 1024);
                        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                            let length = std::str::from_utf8(&request[..end])
                                .unwrap()
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
                    if request.is_empty() {
                        return requests;
                    }
                    requests.push(request);
                    if stall {
                        assert_eq!(stream.read(&mut chunk).await.unwrap_or(0), 0);
                    } else {
                        let _ = stream.write_all(&response).await;
                        let _ = stream.shutdown().await;
                    }
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "client must not retry or follow redirects"
                );
                requests
            })
            .await
            .expect("bounded fixture")
        });
        (client, server)
    }
}

#[tokio::test]
async fn apple_tls_rejects_untrusted_wrong_name_and_client_auth_requirement() {
    for (name, trusted, require_client) in [
        ("provision.example.test", false, false),
        ("wrong.example.test", true, false),
        ("provision.example.test", true, true),
    ] {
        let (client, server) = TlsFixture::new(name, trusted, require_client)
            .serve(vec![response("", b"synthetic")], false)
            .await;
        assert!(
            client
                .post(
                    Url::parse(START).unwrap(),
                    &context(),
                    Zeroizing::new(String::new()),
                    100
                )
                .await
                .is_err()
        );
        assert!(server.await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn apple_https_bounds_status_encoding_headers_and_streamed_body() {
    let large = vec![b'x'; super::super::MAX_RESPONSE_BYTES + 1];
    let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n", large.len(), String::from_utf8(large.clone()).unwrap()).into_bytes();
    for response in [
        b"HTTP/1.1 302 Found\r\nLocation: https://provision.example.test/finish\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec(),
        response("Content-Encoding: gzip\r\n", b"synthetic-private-detail"),
        response("", &large), chunked,
        response(&format!("X-Large: {}\r\n", "x".repeat(MAX_HEADER_BYTES)), b""),
        response(&"X-Many: a\r\n".repeat(MAX_HEADER_COUNT), b""),
        response(&format!("X-Large: {}\r\n", "x".repeat(512 * 1024)), b""),
    ] {
        let (client, server) = TlsFixture::new("provision.example.test", true, false).serve(vec![response], false).await;
        let error = client.post(Url::parse(START).unwrap(), &context(), Zeroizing::new(String::new()), super::super::MAX_RESPONSE_BYTES).await.err().unwrap();
        assert!(!error.contains("synthetic-private-detail"));
        assert_eq!(server.await.unwrap().len(), 1);
    }
}
