use super::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Context;
use tokio::io::ReadBuf;

#[tokio::test]
async fn ownership_lost_during_connect_sends_zero_post_bytes() {
    for has_deadline in [false, true] {
        let admitted = AtomicBool::new(true);
        let owns = || admitted.load(Ordering::SeqCst);
        let (socket, mut peer) = tokio::io::duplex(4096);
        let request = wda_http::request("POST", "/session/fixture/actions", b"{}").unwrap();
        let result = exchange(
            async {
                tokio::task::yield_now().await;
                admitted.store(false, Ordering::SeqCst);
                Ok(socket)
            },
            &request,
            Duration::from_secs(1),
            has_deadline.then(|| Instant::now() + Duration::from_secs(5)),
            Some(&owns),
            Instant::now,
        )
        .await;
        assert!(matches!(result, Err(WdaError::NotConnected)));
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
    }
}

#[derive(Clone, Copy)]
enum LoseAt {
    PendingWrite,
    PartialWrite,
    BeforeFlush,
    PendingFlush,
}

struct LosingSocket {
    admitted: Arc<AtomicBool>,
    at: LoseAt,
    bytes: Rc<Cell<usize>>,
    flushes: Rc<Cell<usize>>,
}

impl AsyncWrite for LosingSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        assert!(self.admitted.load(Ordering::SeqCst));
        assert_eq!(self.bytes.get(), 0, "write retried after ownership loss");
        match self.at {
            LoseAt::PendingWrite => {
                self.admitted.store(false, Ordering::SeqCst);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            LoseAt::PartialWrite => {
                self.admitted.store(false, Ordering::SeqCst);
                self.bytes.set(1);
                Poll::Ready(Ok(1))
            }
            LoseAt::BeforeFlush | LoseAt::PendingFlush => {
                self.bytes.set(bytes.len());
                if matches!(self.at, LoseAt::BeforeFlush) {
                    self.admitted.store(false, Ordering::SeqCst);
                }
                Poll::Ready(Ok(bytes.len()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        assert!(self.admitted.load(Ordering::SeqCst));
        assert!(matches!(self.at, LoseAt::PendingFlush));
        self.flushes.set(self.flushes.get() + 1);
        self.admitted.store(false, Ordering::SeqCst);
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for LosingSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        panic!("refused input must not read a response")
    }
}

#[tokio::test]
async fn ownership_loss_stops_pending_partial_and_flush_polls_without_retry() {
    for at in [
        LoseAt::PendingWrite,
        LoseAt::PartialWrite,
        LoseAt::BeforeFlush,
        LoseAt::PendingFlush,
    ] {
        let admitted = Arc::new(AtomicBool::new(true));
        let owns = || admitted.load(Ordering::SeqCst);
        let bytes = Rc::new(Cell::new(0));
        let flushes = Rc::new(Cell::new(0));
        let socket = LosingSocket {
            admitted: admitted.clone(),
            at,
            bytes: bytes.clone(),
            flushes: flushes.clone(),
        };
        let request = b"POST /session/fixture/actions HTTP/1.1\r\n\r\n";
        let result = exchange(
            async { Ok(socket) },
            request,
            Duration::from_secs(1),
            Some(Instant::now() + Duration::from_secs(5)),
            Some(&owns),
            Instant::now,
        )
        .await;
        let expected_bytes = match at {
            LoseAt::PendingWrite => 0,
            LoseAt::PartialWrite => 1,
            _ => request.len(),
        };
        assert_eq!(bytes.get(), expected_bytes);
        assert_eq!(
            flushes.get(),
            usize::from(matches!(at, LoseAt::PendingFlush))
        );
        assert!(matches!(
            (result, at),
            (Err(WdaError::NotConnected), LoseAt::PendingWrite) | (Err(WdaError::AfterSent), _)
        ));
    }
}
