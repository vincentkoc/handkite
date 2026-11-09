use crate::usb::{PinnedUsb, UsbError};
use crate::wda_http::{self, HttpError, HttpResponse};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WDA_PORT: u16 = 8100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WIRE: usize = wda_http::MAX_BODY + 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WdaError {
    Usb(UsbError),
    InvalidRequest(HttpError),
    NotConnected,
    AfterSent,
    TimedOutAfterSent,
}

pub struct UsbWda {
    usb: PinnedUsb,
}

impl UsbWda {
    pub fn new(usb: PinnedUsb) -> Self {
        Self { usb }
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError> {
        let request = wda_http::request(method, path, body).map_err(WdaError::InvalidRequest)?;
        let provider = self.usb.provider().await.map_err(WdaError::Usb)?;
        let device = tokio::time::timeout(CONNECT_TIMEOUT, provider.connect(WDA_PORT))
            .await
            .map_err(|_| WdaError::NotConnected)?
            .map_err(|_| WdaError::NotConnected)?;
        let mut socket = device.get_socket().ok_or(WdaError::NotConnected)?;
        tokio::time::timeout(deadline, async {
            // A write error can follow a partial request; never retry an action automatically.
            socket
                .write_all(&request)
                .await
                .map_err(|_| WdaError::AfterSent)?;
            socket.flush().await.map_err(|_| WdaError::AfterSent)?;
            let mut raw = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let count = socket
                    .read(&mut chunk)
                    .await
                    .map_err(|_| WdaError::AfterSent)?;
                if count == 0 {
                    return Err(WdaError::AfterSent);
                }
                if raw.len() + count > MAX_WIRE {
                    return Err(WdaError::AfterSent);
                }
                raw.extend_from_slice(&chunk[..count]);
                match wda_http::parse_response(&raw) {
                    Ok(Some(response)) => return Ok(response),
                    Ok(None) => {}
                    Err(_) => return Err(WdaError::AfterSent),
                }
            }
        })
        .await
        .map_err(|_| WdaError::TimedOutAfterSent)?
    }
}
