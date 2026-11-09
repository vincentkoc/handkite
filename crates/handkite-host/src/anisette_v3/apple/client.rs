use super::{AppleProvisioningContext, MAX_HEADER_BYTES, MAX_HEADER_COUNT};
use futures_util::StreamExt;
use reqwest::{
    Url,
    header::{CONTENT_ENCODING, HeaderMap},
};
use rustls::{ClientConfig, RootCertStore};
use std::{sync::Arc, time::Duration};
use zeroize::Zeroizing;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_FAILED: &str = "Apple provisioning HTTPS request failed";
const RESPONSE_FAILED: &str = "Apple provisioning HTTPS response is invalid";

/// Deliberately separate from the sidecar's client-certificate transport.
pub(super) struct AppleClient {
    http: reqwest::Client,
    #[cfg(test)]
    test_port: Option<u16>,
}

pub(super) struct AppleResponse {
    pub body: Zeroizing<Vec<u8>>,
    pub headers: HeaderMap,
}

impl AppleClient {
    pub fn new() -> Result<Self, &'static str> {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(Self {
            http: builder(roots)?.build().map_err(|_| REQUEST_FAILED)?,
            #[cfg(test)]
            test_port: None,
        })
    }

    pub async fn lookup(
        &self,
        url: Url,
        context: &AppleProvisioningContext,
        limit: usize,
    ) -> Result<AppleResponse, &'static str> {
        self.response(
            self.http.get(self.url(url)).headers(context.headers(true)?),
            limit,
        )
        .await
    }

    pub async fn post(
        &self,
        url: Url,
        context: &AppleProvisioningContext,
        body: Zeroizing<String>,
        limit: usize,
    ) -> Result<AppleResponse, &'static str> {
        self.response(
            self.http
                .post(self.url(url))
                .headers(context.headers(false)?)
                .body(body.as_bytes().to_vec()),
            limit,
        )
        .await
    }

    fn url(&self, url: Url) -> Url {
        #[cfg(test)]
        if let Some(port) = self.test_port {
            let mut url = url;
            url.set_port(Some(port)).unwrap();
            return url;
        }
        url
    }

    async fn response(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<AppleResponse, &'static str> {
        tokio::time::timeout(HTTP_TIMEOUT, async {
            let response = request.send().await.map_err(|_| REQUEST_FAILED)?;
            if response.status() != reqwest::StatusCode::OK
                || response.headers().contains_key(CONTENT_ENCODING)
                || response
                    .content_length()
                    .is_some_and(|length| length > limit as u64)
            {
                return Err(RESPONSE_FAILED);
            }
            // This is an accepted-header limit after Hyper's bounded receive parsing.
            // It is not a 16 KiB transport allocation limit.
            let headers = response.headers();
            if headers.len() > MAX_HEADER_COUNT {
                return Err(RESPONSE_FAILED);
            }
            let mut bytes = 0usize;
            for (name, value) in headers {
                bytes = bytes
                    .checked_add(name.as_str().len())
                    .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
                    .filter(|bytes| *bytes <= MAX_HEADER_BYTES)
                    .ok_or(RESPONSE_FAILED)?;
            }
            let headers = headers.clone();
            let mut stream = response.bytes_stream();
            let mut body = Zeroizing::new(Vec::new());
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| RESPONSE_FAILED)?;
                if chunk.len() > limit - body.len() {
                    return Err(RESPONSE_FAILED);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(AppleResponse { body, headers })
        })
        .await
        .map_err(|_| REQUEST_FAILED)?
    }
}

fn builder(roots: RootCertStore) -> Result<reqwest::ClientBuilder, &'static str> {
    let mut tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| REQUEST_FAILED)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .https_only(true)
        .http1_only()
        .http1_max_headers(MAX_HEADER_COUNT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        // Hyper can retry cancelled requests on reused sockets independently of reqwest.
        .pool_max_idle_per_host(0)
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(10))
        .timeout(HTTP_TIMEOUT)
        .read_timeout(HTTP_TIMEOUT))
}

#[cfg(test)]
pub(super) mod tests;
