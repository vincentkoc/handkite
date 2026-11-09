use super::{
    AnisetteIdentity, AppleSigningHeaders, MAX_HEADERS_RESPONSE_BYTES, MacosProvisionedState,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zeroize::Zeroizing;

const CLIENT_INFO_URL: &str = "https://anisette.example.invalid/v3/client_info";
const HEADERS_URL: &str = "https://anisette.example.invalid/v3/get_headers";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

    fn parse(body: &[u8]) -> Result<Self, &'static str> {
        let info: Self =
            serde_json::from_slice(body).map_err(|_| "Anisette client information is invalid")?;
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
    #[cfg(test)]
    test_port: Option<u16>,
}

impl AnisetteClient {
    pub fn new(identity: AnisetteIdentity) -> Result<Self, &'static str> {
        Ok(Self {
            http: http_builder(&identity)
                .build()
                .map_err(|_| "Anisette HTTPS configuration failed")?,
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
