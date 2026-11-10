use crate::usb::{PinnedUsb, UsbError};
use crate::wda_http::{self, HttpError, HttpResponse};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const WDA_PORT: u16 = 8100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WIRE: usize = wda_http::MAX_BODY + 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WdaError {
    Usb(UsbError),
    InvalidRequest(HttpError),
    NotConnected,
    ExpiredBeforeSend,
    AfterSent,
    TimedOutAfterSent,
}

pub struct UsbWda {
    usb: PinnedUsb,
    write_deadline: Option<Instant>,
    write_admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl UsbWda {
    pub fn new(usb: PinnedUsb) -> Self {
        Self {
            usb,
            write_deadline: None,
            write_admission: None,
        }
    }

    /// Bound an input's freshness through discovery, connect and writes.
    /// Existing transports remain unbounded unless explicitly wrapped here.
    pub fn with_write_deadline(&self, deadline: Instant) -> Self {
        Self {
            usb: self.usb.clone(),
            write_deadline: Some(
                self.write_deadline
                    .map_or(deadline, |old| old.min(deadline)),
            ),
            write_admission: self.write_admission.clone(),
        }
    }

    /// Retain input ownership checks through asynchronous connect and socket polls.
    pub(crate) fn with_write_admission(
        mut self,
        admission: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        self.write_admission = admission;
        self
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError> {
        let request = wda_http::request(method, path, body).map_err(WdaError::InvalidRequest)?;
        let connect = async {
            let provider = self.usb.provider().await.map_err(WdaError::Usb)?;
            let device = tokio::time::timeout(CONNECT_TIMEOUT, provider.connect(WDA_PORT))
                .await
                .map_err(|_| WdaError::NotConnected)?
                .map_err(|_| WdaError::NotConnected)?;
            device.get_socket().ok_or(WdaError::NotConnected)
        };
        exchange(
            connect,
            &request,
            deadline,
            self.write_deadline,
            self.write_admission.as_deref().filter(|_| method != "GET"),
            Instant::now,
        )
        .await
    }
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    connect: impl Future<Output = Result<S, WdaError>>,
    request: &[u8],
    deadline: Duration,
    write_deadline: Option<Instant>,
    admission: Option<&(dyn Fn() -> bool + Send + Sync)>,
    now: impl Fn() -> Instant,
) -> Result<HttpResponse, WdaError> {
    let mut socket = connect.await?;
    tokio::time::timeout(deadline, async {
        if write_deadline.is_some() || admission.is_some() {
            let mut sent = 0;
            while sent < request.len() {
                // Check on every write poll, including after a pending socket wakes.
                let count = poll_fn(|cx| {
                    let expired = write_deadline.is_some_and(|expires| now() >= expires);
                    let refused = admission.is_some_and(|admit| !admit());
                    if expired || refused {
                        return Poll::Ready(Err(if sent == 0 {
                            if expired {
                                WdaError::ExpiredBeforeSend
                            } else {
                                WdaError::NotConnected
                            }
                        } else {
                            WdaError::AfterSent
                        }));
                    }
                    Pin::new(&mut socket)
                        .poll_write(cx, &request[sent..])
                        .map_err(|_| WdaError::AfterSent)
                })
                .await?;
                if count == 0 {
                    return Err(WdaError::AfterSent);
                }
                sent += count;
            }
        } else {
            // A write error can follow a partial request; never retry an action automatically.
            socket
                .write_all(request)
                .await
                .map_err(|_| WdaError::AfterSent)?;
        }
        if write_deadline.is_some() || admission.is_some() {
            poll_fn(|cx| {
                if write_deadline.is_some_and(|expires| now() >= expires)
                    || admission.is_some_and(|admit| !admit())
                {
                    return Poll::Ready(Err(WdaError::AfterSent));
                }
                Pin::new(&mut socket)
                    .poll_flush(cx)
                    .map_err(|_| WdaError::AfterSent)
            })
            .await?;
        } else {
            socket.flush().await.map_err(|_| WdaError::AfterSent)?;
        }
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

#[cfg(test)]
#[path = "wda_transport/ownership_tests.rs"]
mod ownership_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Context;
    use tokio::io::ReadBuf;

    #[tokio::test]
    async fn delayed_discovery_or_connect_cannot_send_an_expired_post() {
        for elapsed_ms in [1_000, 2_000, 30_000] {
            let started = Instant::now();
            let clock = Cell::new(started);
            let expires = started + Duration::from_secs(2);
            let request = wda_http::request("POST", "/session/fixture/actions", b"{}").unwrap();
            let (socket, mut peer) = tokio::io::duplex(4096);
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"value\":null}")
                .await
                .unwrap();
            let connect = async {
                // Advance a controlled monotonic clock while discovery/connect is pending.
                clock.set(started + Duration::from_millis(elapsed_ms));
                Ok(socket)
            };
            let result = exchange(
                connect,
                &request,
                Duration::from_secs(1),
                Some(expires),
                None,
                || clock.get(),
            )
            .await;
            let mut sent = Vec::new();
            peer.read_to_end(&mut sent).await.unwrap();
            if elapsed_ms < 2_000 {
                assert!(result.is_ok());
                assert_eq!(sent, request);
            } else {
                assert!(matches!(result, Err(WdaError::ExpiredBeforeSend)));
                assert!(sent.is_empty(), "expired connection sent POST bytes");
            }
        }
    }

    struct DelayedWriter {
        clock: Rc<Cell<Instant>>,
        expires: Instant,
        bytes: Rc<Cell<usize>>,
        partial: bool,
    }

    impl AsyncWrite for DelayedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            assert!(
                self.clock.get() < self.expires,
                "expired write was polled again"
            );
            self.clock.set(self.expires);
            if self.partial {
                self.bytes.set(1);
                Poll::Ready(Ok(1))
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            panic!("expired input cannot flush");
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for DelayedWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            panic!("expired input cannot read a response");
        }
    }

    #[tokio::test]
    async fn deadline_survives_pending_or_partial_writes_without_retry() {
        for partial in [false, true] {
            let started = Instant::now();
            let clock = Rc::new(Cell::new(started));
            let bytes = Rc::new(Cell::new(0));
            let expires = started + Duration::from_secs(2);
            let socket = DelayedWriter {
                clock: clock.clone(),
                expires,
                bytes: bytes.clone(),
                partial,
            };
            let result = exchange(
                async { Ok(socket) },
                b"POST / HTTP/1.1\r\n\r\n",
                Duration::from_secs(1),
                Some(expires),
                None,
                || clock.get(),
            )
            .await;
            if partial {
                assert!(matches!(result, Err(WdaError::AfterSent)));
                assert_eq!(bytes.get(), 1);
            } else {
                assert!(matches!(result, Err(WdaError::ExpiredBeforeSend)));
                assert_eq!(bytes.get(), 0);
            }
        }
    }

    struct DelayedFlush {
        clock: Rc<Cell<Instant>>,
        expires: Instant,
        bytes: Rc<Cell<usize>>,
        flushes: Rc<Cell<usize>>,
        pending_flush: bool,
    }

    impl AsyncWrite for DelayedFlush {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            assert_eq!(self.bytes.get(), 0, "input was sent again");
            self.bytes.set(bytes.len());
            if !self.pending_flush {
                self.clock.set(self.expires);
            }
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            assert!(self.clock.get() < self.expires, "expired flush was polled");
            self.flushes.set(self.flushes.get() + 1);
            self.clock.set(self.expires);
            cx.waker().wake_by_ref();
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for DelayedFlush {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            panic!("expired flush cannot read a response");
        }
    }

    #[tokio::test]
    async fn deadline_survives_expiry_before_or_during_flush_without_retry() {
        for pending_flush in [false, true] {
            let started = Instant::now();
            let clock = Rc::new(Cell::new(started));
            let bytes = Rc::new(Cell::new(0));
            let flushes = Rc::new(Cell::new(0));
            let expires = started + Duration::from_secs(5);
            let socket = DelayedFlush {
                clock: clock.clone(),
                expires,
                bytes: bytes.clone(),
                flushes: flushes.clone(),
                pending_flush,
            };
            let request = b"POST /session/fixture/actions HTTP/1.1\r\n\r\n";
            assert!(matches!(
                exchange(
                    async { Ok(socket) },
                    request,
                    Duration::from_secs(1),
                    Some(expires),
                    None,
                    || clock.get(),
                )
                .await,
                Err(WdaError::AfterSent)
            ));
            assert_eq!(bytes.get(), request.len());
            assert_eq!(flushes.get(), usize::from(pending_flush));
        }
    }
}
