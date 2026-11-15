use super::{DeveloperCredential, request};
use crate::anisette_v3::{AnisetteClient, state};
use futures_util::StreamExt;
use plist::{Dictionary, Value};
use reqwest::header::CONTENT_ENCODING;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

const ENDPOINT: &str = "https://developerservices2.apple.com/services/QH65B2/listTeams.action";
const INVALID: &str = "developer team response is invalid";
const FAILED: &str = "developer team request failed";
const MAX_RESPONSE: usize = 128 * 1024;
const MAX_HEADERS: usize = 128;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);

/// Experimental, read-only compatibility probe for an undocumented endpoint.
/// A successful response does not sign or install an application.
pub async fn list_teams(
    anisette: &AnisetteClient,
    credential: DeveloperCredential,
) -> Result<(), &'static str> {
    let state = state::load_ready()?;
    let info = anisette.client_info().await?;
    let headers = anisette.get_headers(&state).await?;
    let request_headers = request::headers(&credential, &state, &info, &headers)?;
    let request_body = request_body()?;
    let http = builder(RootCertStore::from_iter(
        webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
    ))?
    .build()
    .map_err(|_| FAILED)?;
    send(&http, ENDPOINT, request_headers, request_body).await
}

fn request_body() -> Result<Zeroizing<Vec<u8>>, &'static str> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut id = [0_u8; 16];
    SystemRandom::new().fill(&mut id).map_err(|_| FAILED)?;
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    let id = format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        u32::from_be_bytes(id[0..4].try_into().map_err(|_| FAILED)?),
        u16::from_be_bytes(id[4..6].try_into().map_err(|_| FAILED)?),
        u16::from_be_bytes(id[6..8].try_into().map_err(|_| FAILED)?),
        u16::from_be_bytes(id[8..10].try_into().map_err(|_| FAILED)?),
        u64::from_be_bytes([0, 0, id[10], id[11], id[12], id[13], id[14], id[15]])
    );
    let mut body = Dictionary::new();
    body.insert("requestId".into(), Value::String(id));
    let mut bytes = Zeroizing::new(Vec::new());
    plist::to_writer_xml(&mut *bytes, &Value::Dictionary(body)).map_err(|_| FAILED)?;
    Ok(bytes)
}

async fn send(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    body: Zeroizing<Vec<u8>>,
) -> Result<(), &'static str> {
    tokio::time::timeout(DEADLINE, async {
        let response = client
            .post(url)
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| FAILED)?;
        if response.status() != reqwest::StatusCode::OK
            || response.headers().contains_key(CONTENT_ENCODING)
            || response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE as u64)
            || response.headers().len() > MAX_HEADERS
        {
            return Err(INVALID);
        }
        let mut header_bytes = 0_usize;
        for (name, value) in response.headers() {
            header_bytes = header_bytes
                .checked_add(name.as_str().len())
                .and_then(|length| length.checked_add(value.as_bytes().len()))
                .filter(|length| *length <= MAX_HEADER_BYTES)
                .ok_or(INVALID)?;
        }
        let mut body = Zeroizing::new(Vec::new());
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| FAILED)?;
            if chunk.len() > MAX_RESPONSE - body.len() {
                return Err(INVALID);
            }
            body.extend_from_slice(&chunk);
        }
        parse_response(&body)
    })
    .await
    .map_err(|_| FAILED)?
}

fn parse_response(body: &[u8]) -> Result<(), &'static str> {
    if body.is_empty() || body.len() > MAX_RESPONSE {
        return Err(INVALID);
    }
    let root: Value = plist::from_bytes(body).map_err(|_| INVALID)?;
    let fields = root.as_dictionary().ok_or(INVALID)?;
    if fields.get("resultCode").and_then(Value::as_signed_integer) != Some(0) {
        return Err(INVALID);
    }
    let teams = fields
        .get("teams")
        .and_then(Value::as_array)
        .ok_or(INVALID)?;
    if teams.is_empty() || teams.len() > 128 {
        return Err(INVALID);
    }
    for team in teams {
        let id = team
            .as_dictionary()
            .and_then(|team| team.get("teamId"))
            .and_then(Value::as_string)
            .ok_or(INVALID)?;
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(INVALID);
        }
    }
    Ok(())
}

fn builder(roots: RootCertStore) -> Result<reqwest::ClientBuilder, &'static str> {
    let mut tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| FAILED)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .https_only(true)
        .http1_only()
        .http1_max_headers(MAX_HEADERS)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(10))
        .timeout(DEADLINE)
        .read_timeout(DEADLINE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
    use rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_rustls::TlsAcceptor;

    fn response(result: i64, teams: Vec<Value>) -> Vec<u8> {
        let mut root = Dictionary::new();
        root.insert("resultCode".into(), Value::Integer(result.into()));
        root.insert("teams".into(), Value::Array(teams));
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &Value::Dictionary(root)).unwrap();
        bytes
    }

    #[test]
    fn validates_success_and_rejects_empty_or_failed_team_lists() {
        let mut team = Dictionary::new();
        team.insert("teamId".into(), Value::String("TEAM123".into()));
        let good = response(0, vec![Value::Dictionary(team)]);
        assert_eq!(parse_response(&good), Ok(()));
        assert!(parse_response(&response(1, vec![])).is_err());
        assert!(parse_response(&response(0, vec![])).is_err());
        assert!(parse_response(b"not plist").is_err());
    }

    #[test]
    fn request_id_is_uppercase_uuid_v4() {
        let body = request_body().unwrap();
        let value: Value = plist::from_bytes(&body).unwrap();
        let id = value
            .as_dictionary()
            .and_then(|value| value.get("requestId"))
            .and_then(Value::as_string)
            .unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'A' | b'B'));
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
        );
    }

    #[tokio::test]
    async fn fixed_host_request_uses_loopback_tls_and_validates_result() {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = Certificate::from_params(ca_params).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(ca.serialize_der().unwrap().into()).unwrap();
        let mut server_params = CertificateParams::new(vec!["developerservices2.apple.com".into()]);
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = Certificate::from_params(server_params).unwrap();
        let key: PrivatePkcs8KeyDer<'static> = certificate.serialize_private_key_der().into();
        let der: CertificateDer<'static> =
            certificate.serialize_der_with_signer(&ca).unwrap().into();
        let crypto = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = ServerConfig::builder_with_provider(crypto)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der], key.into())
            .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let http = builder(roots)
            .unwrap()
            .resolve("developerservices2.apple.com", address)
            .build()
            .unwrap();
        let response = response(
            0,
            vec![Value::Dictionary({
                let mut team = Dictionary::new();
                team.insert("teamId".into(), Value::String("TEAM123".into()));
                team
            })],
        );
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = TlsAcceptor::from(Arc::new(server))
                .accept(tcp)
                .await
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                assert!(request.len() < 16 * 1024);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.write_all(&response).await.unwrap();
            request
        });
        let mut headers = reqwest::header::HeaderMap::new();
        let mut token = reqwest::header::HeaderValue::from_static("synthetic-token");
        token.set_sensitive(true);
        headers.insert("x-apple-gs-token", token);
        let url = ENDPOINT.replace(
            "apple.com/services",
            &format!("apple.com:{}/services", address.port()),
        );
        assert_eq!(
            send(&http, &url, headers, request_body().unwrap()).await,
            Ok(())
        );
        let request = server.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /services/QH65B2/listTeams.action HTTP/1.1\r\n"));
        assert!(
            text.to_ascii_lowercase()
                .contains("x-apple-gs-token: synthetic-token\r\n")
        );
        assert!(text.contains("requestId"));
    }
}
