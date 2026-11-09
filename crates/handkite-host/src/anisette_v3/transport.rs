use super::{
    AnisetteIdentity, AppleSigningHeaders, FinishProvisioning, MAX_CPIM_BYTES,
    MAX_HEADERS_RESPONSE_BYTES, MAX_SERVER_MESSAGE_BYTES, MacosProvisionedState,
    ProvisioningConversation, ProvisioningEvent, RoutingInfo, validate_base64,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

const CLIENT_INFO_URL: &str = "https://anisette.example.invalid/v3/client_info";
const HEADERS_URL: &str = "https://anisette.example.invalid/v3/get_headers";
const PROVISIONING_URL: &str = "wss://anisette.example.invalid/v3/provisioning_session";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const STAGE_TIMEOUT: Duration = Duration::from_secs(3);
const CONVERSATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONTROL_FRAMES: usize = 16;
const MAX_CLIENT_MESSAGE_BYTES: usize = 2 * MAX_CPIM_BYTES.div_ceil(3) * 4 + 1024;

pub struct StartProvisioningData {
    spim: Zeroizing<String>,
}

impl StartProvisioningData {
    pub fn new(spim: String) -> Result<Self, &'static str> {
        let spim = Zeroizing::new(spim);
        validate_base64(&spim, MAX_CPIM_BYTES)?;
        Ok(Self { spim })
    }
}

pub struct EndProvisioningData {
    ptm: Zeroizing<String>,
    tk: Zeroizing<String>,
    routing_info: RoutingInfo,
}

impl EndProvisioningData {
    pub fn new(ptm: String, tk: String, routing_info: RoutingInfo) -> Result<Self, &'static str> {
        let ptm = Zeroizing::new(ptm);
        let tk = Zeroizing::new(tk);
        validate_base64(&ptm, MAX_CPIM_BYTES)?;
        validate_base64(&tk, MAX_CPIM_BYTES)?;
        Ok(Self {
            ptm,
            tk,
            routing_info,
        })
    }
}

#[derive(Debug)]
pub struct ProvisioningDataError;

/// Supplies only the typed V3 fields. Apple credential handling and requests
/// belong to a separate implementation; this transport never performs them.
pub trait ProvisioningDataProvider {
    fn start(
        &mut self,
    ) -> impl Future<Output = Result<StartProvisioningData, ProvisioningDataError>>;
    fn finish(
        &mut self,
        cpim: &str,
    ) -> impl Future<Output = Result<EndProvisioningData, ProvisioningDataError>>;
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct ClientInfo {
    client_info: String,
    user_agent: String,
}

impl ClientInfo {
    pub fn client_info(&self) -> &str {
        &self.client_info
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub(super) fn parse(body: &[u8]) -> Result<Self, &'static str> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            client_info: String,
            user_agent: String,
        }
        let fields: Fields =
            serde_json::from_slice(body).map_err(|_| "Anisette client information is invalid")?;
        let info = Self {
            client_info: fields.client_info,
            user_agent: fields.user_agent,
        };
        if [&info.client_info, &info.user_agent].iter().any(|value| {
            value.is_empty()
                || value.len() > 4096
                || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        }) {
            return Err("Anisette client information is invalid");
        }
        Ok(info)
    }
}

/// Fixed-origin V3 transport. It has no endpoint, proxy or retry configuration.
pub struct AnisetteClient {
    http: reqwest::Client,
    tls: Arc<ClientConfig>,
    #[cfg(test)]
    test_port: Option<u16>,
}

impl AnisetteClient {
    pub fn new(identity: AnisetteIdentity) -> Result<Self, &'static str> {
        Ok(Self {
            http: http_builder(&identity)
                .build()
                .map_err(|_| "Anisette HTTPS configuration failed")?,
            tls: identity.config,
            #[cfg(test)]
            test_port: None,
        })
    }

    pub async fn client_info(&self) -> Result<ClientInfo, &'static str> {
        let body = self
            .response(self.http.get(self.url(CLIENT_INFO_URL)))
            .await?;
        ClientInfo::parse(&body)
    }

    /// The identifier comes from the validated state, never a second caller input.
    pub async fn get_headers(
        &self,
        state: &MacosProvisionedState,
    ) -> Result<AppleSigningHeaders, &'static str> {
        #[derive(Serialize)]
        struct Request<'a> {
            identifier: &'a str,
            adi_pb: &'a str,
        }
        let identifier = STANDARD.encode(state.identifier);
        let adi_pb = Zeroizing::new(STANDARD.encode(state.as_bytes()));
        let body = serde_json::to_vec(&Request {
            identifier: &identifier,
            adi_pb: &adi_pb,
        })
        .map_err(|_| "Anisette header request is invalid")?;
        let request = self
            .http
            .post(self.url(HEADERS_URL))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let body = self.response(request).await?;
        AppleSigningHeaders::parse(200, &body)
    }

    /// One conversation, with no replay after an error, cancellation or timeout.
    pub async fn provision<P: ProvisioningDataProvider>(
        &self,
        identifier: [u8; 16],
        provider: &mut P,
    ) -> Result<MacosProvisionedState, &'static str> {
        self.provision_until(identifier, provider, Instant::now() + CONVERSATION_TIMEOUT)
            .await
    }

    /// Lookup and exact endpoint approval complete before opening the sidecar WSS.
    /// The production policy currently rejects this operation before any network IO.
    pub async fn provision_apple(
        &self,
        context: super::apple::AppleProvisioningContext,
    ) -> Result<MacosProvisionedState, &'static str> {
        let mut provider = super::apple::AppleProvisioningProvider::discover(context).await?;
        self.provision(*provider.identifier().as_bytes(), &mut provider)
            .await
    }

    async fn provision_until<P: ProvisioningDataProvider>(
        &self,
        identifier: [u8; 16],
        provider: &mut P,
        conversation_deadline: Instant,
    ) -> Result<MacosProvisionedState, &'static str> {
        let mut socket = timeout_at(conversation_deadline, self.connect())
            .await
            .map_err(|_| "Anisette provisioning conversation timed out")??;
        let mut conversation = ProvisioningConversation::default();
        loop {
            let read_deadline = (Instant::now() + STAGE_TIMEOUT).min(conversation_deadline);
            let message = receive(&mut socket, read_deadline).await?;
            let deadline = (Instant::now() + STAGE_TIMEOUT).min(conversation_deadline);
            match conversation.accept(&message)? {
                ProvisioningEvent::GiveIdentifier => {
                    #[derive(Serialize)]
                    struct Identifier {
                        identifier: String,
                    }
                    send(
                        &mut socket,
                        &Identifier {
                            identifier: STANDARD.encode(identifier),
                        },
                        deadline,
                    )
                    .await?;
                }
                ProvisioningEvent::GiveStartProvisioningData => {
                    let data = timeout_at(deadline, provider.start())
                        .await
                        .map_err(|_| "Anisette provisioning stage timed out")?
                        .map_err(|_| "Anisette provisioning data unavailable")?;
                    #[derive(Serialize)]
                    struct Start<'a> {
                        spim: &'a str,
                    }
                    send(&mut socket, &Start { spim: &data.spim }, deadline).await?;
                }
                ProvisioningEvent::GiveEndProvisioningData { cpim } => {
                    let cpim = Zeroizing::new(cpim);
                    let data = timeout_at(deadline, provider.finish(&cpim))
                        .await
                        .map_err(|_| "Anisette provisioning stage timed out")?
                        .map_err(|_| "Anisette provisioning data unavailable")?;
                    send(
                        &mut socket,
                        &FinishProvisioning::new(&data.ptm, &data.tk, data.routing_info),
                        deadline,
                    )
                    .await?;
                }
                ProvisioningEvent::ProvisioningSuccess { adi_pb } => {
                    let state = MacosProvisionedState::parse(adi_pb, identifier)?;
                    timeout_at(deadline, socket.close(None))
                        .await
                        .map_err(|_| "Anisette provisioning stage timed out")?
                        .map_err(|_| "Anisette provisioning close failed")?;
                    return Ok(state);
                }
            }
        }
    }

    async fn connect(&self) -> Result<Socket, &'static str> {
        timeout(CONNECT_TIMEOUT, async {
            #[cfg(test)]
            let tcp = if let Some(port) = self.test_port {
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await
            } else {
                TcpStream::connect(("anisette.example.invalid", 443)).await
            };
            #[cfg(not(test))]
            let tcp = TcpStream::connect(("anisette.example.invalid", 443)).await;
            let tcp = tcp.map_err(|_| "Anisette WSS connection failed")?;
            let config = WebSocketConfig::default()
                .read_buffer_size(16 * 1024)
                .write_buffer_size(0)
                .max_write_buffer_size(MAX_CLIENT_MESSAGE_BYTES)
                .max_message_size(Some(MAX_SERVER_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_SERVER_MESSAGE_BYTES));
            let (socket, _) = tokio_tungstenite::client_async_tls_with_config(
                self.url(PROVISIONING_URL),
                tcp,
                Some(config),
                Some(Connector::Rustls(self.tls.clone())),
            )
            .await
            .map_err(|_| "Anisette WSS handshake failed")?;
            Ok(socket)
        })
        .await
        .map_err(|_| "Anisette WSS connection timed out")?
    }

    fn url(&self, endpoint: &'static str) -> String {
        #[cfg(test)]
        if let Some(port) = self.test_port {
            let mut url = reqwest::Url::parse(endpoint).expect("constant URL");
            url.set_port(Some(port)).unwrap();
            return url.into();
        }
        endpoint.into()
    }

    async fn response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Zeroizing<Vec<u8>>, &'static str> {
        tokio::time::timeout(HTTP_TIMEOUT, async {
            let response = request
                .send()
                .await
                .map_err(|_| "Anisette HTTPS request failed")?;
            if response.status() != reqwest::StatusCode::OK {
                return Err("Anisette HTTPS response failed");
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_HEADERS_RESPONSE_BYTES as u64)
            {
                return Err("Anisette HTTPS response is too large");
            }
            let mut stream = response.bytes_stream();
            let mut body = Zeroizing::new(Vec::new());
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| "Anisette HTTPS response failed")?;
                if chunk.len() > MAX_HEADERS_RESPONSE_BYTES - body.len() {
                    return Err("Anisette HTTPS response is too large");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
        .await
        .map_err(|_| "Anisette HTTPS request timed out")?
    }
}

async fn receive(
    socket: &mut Socket,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    timeout_at(deadline, async {
        let mut controls = 0;
        loop {
            let message = socket
                .next()
                .await
                .ok_or("Anisette provisioning closed early")?
                .map_err(|_| "Anisette provisioning receive failed")?;
            match message {
                Message::Text(text) => return Ok(Zeroizing::new(text.as_bytes().to_vec())),
                Message::Ping(_) | Message::Pong(_) => {
                    controls += 1;
                    if controls > MAX_CONTROL_FRAMES {
                        return Err("Anisette provisioning control frame limit exceeded");
                    }
                    socket
                        .flush()
                        .await
                        .map_err(|_| "Anisette provisioning send failed")?;
                }
                Message::Close(_) => return Err("Anisette provisioning closed early"),
                _ => return Err("Anisette provisioning requires text messages"),
            }
        }
    })
    .await
    .map_err(|_| "Anisette provisioning stage timed out")?
}

async fn send<T: Serialize>(
    socket: &mut Socket,
    value: &T,
    deadline: Instant,
) -> Result<(), &'static str> {
    let message = Zeroizing::new(
        serde_json::to_string(value).map_err(|_| "Anisette provisioning request is invalid")?,
    );
    if message.len() > MAX_CLIENT_MESSAGE_BYTES {
        return Err("Anisette provisioning request is too large");
    }
    timeout_at(
        deadline,
        socket.send(Message::Text(message.to_string().into())),
    )
    .await
    .map_err(|_| "Anisette provisioning stage timed out")?
    .map_err(|_| "Anisette provisioning send failed")
}

fn http_builder(identity: &AnisetteIdentity) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .tls_backend_preconfigured((*identity.config).clone())
        .https_only(true)
        .http1_only()
        .http1_max_headers(32)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .read_timeout(HTTP_TIMEOUT)
}

#[cfg(test)]
mod tests;
