use crate::wda_action::WdaRequest;
use crate::wda_http::HttpResponse;
use crate::wda_transport::WdaError;
use std::time::Duration;

pub(super) struct InputRequests<'a, W> {
    pub wda: &'a W,
    pub admission: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
}

impl<W: WdaRequest> WdaRequest for InputRequests<'_, W> {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        deadline: Duration,
    ) -> Result<HttpResponse, WdaError> {
        // WdaActions validates the session asynchronously. Recheck at the write
        // boundary so ownership lost during those reads cannot admit input.
        if method != "GET" && self.admission.is_some_and(|admit| !admit()) {
            return Err(WdaError::NotConnected);
        }
        self.wda.request(method, path, body, deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wda_action::WdaActions;
    use handkite_core::protocol::{Action, Point};
    use serde_json::json;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Fake<'a> {
        ownership: &'a AtomicBool,
        lose_on_validation: bool,
        writes: Cell<usize>,
    }

    impl WdaRequest for Fake<'_> {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            let value = match (method, path) {
                ("GET", "/status") => {
                    json!({"sessionId":"fixture-session","value":{"ready":true}})
                }
                ("GET", "/session/fixture-session") => {
                    if self.lose_on_validation {
                        self.ownership.store(false, Ordering::SeqCst);
                    }
                    json!({"value":{}})
                }
                ("POST", _) => {
                    self.writes.set(self.writes.get() + 1);
                    json!({"value":null})
                }
                _ => panic!("unexpected fixture request"),
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&value).unwrap(),
            })
        }
    }

    async fn input(kind: usize, wda: &impl WdaRequest) -> bool {
        let mut actions = WdaActions::bound_to_existing("fixture-session").unwrap();
        let at = Point { x: 100.0, y: 200.0 };
        match kind {
            0 => actions.wake_with_existing_session(wda).await.is_ok(),
            1 => actions.tap_with_existing_session(wda, at).await.is_ok(),
            2 => actions
                .execute_with_existing_session(wda, &Action::Tap { at })
                .await
                .is_ok(),
            3 => actions
                .execute_with_existing_session(
                    wda,
                    &Action::Swipe {
                        from: at,
                        to: Point { x: 100.0, y: 80.0 },
                        duration_ms: 300,
                    },
                )
                .await
                .is_ok(),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn lost_runner_or_lock_during_validation_prevents_every_input_write() {
        for runner_lost in [false, true] {
            for kind in 0..4 {
                let runner = AtomicBool::new(true);
                let lock = AtomicBool::new(true);
                let fake = Fake {
                    ownership: if runner_lost { &runner } else { &lock },
                    lose_on_validation: true,
                    writes: Cell::new(0),
                };
                let admit = || runner.load(Ordering::SeqCst) && lock.load(Ordering::SeqCst);
                let guarded = InputRequests {
                    wda: &fake,
                    admission: Some(&admit),
                };
                assert!(!input(kind, &guarded).await);
                assert_eq!(fake.writes.get(), 0);
            }
        }
    }

    #[tokio::test]
    async fn lost_ownership_after_an_input_blocks_the_next_write() {
        let ownership = AtomicBool::new(true);
        let fake = Fake {
            ownership: &ownership,
            lose_on_validation: false,
            writes: Cell::new(0),
        };
        let admit = || ownership.load(Ordering::SeqCst);
        let guarded = InputRequests {
            wda: &fake,
            admission: Some(&admit),
        };
        assert!(input(2, &guarded).await);
        ownership.store(false, Ordering::SeqCst);
        assert!(!input(2, &guarded).await);
        assert_eq!(fake.writes.get(), 1);
    }

    #[tokio::test]
    async fn absent_admission_preserves_existing_session_inputs() {
        let ownership = AtomicBool::new(false);
        let fake = Fake {
            ownership: &ownership,
            lose_on_validation: false,
            writes: Cell::new(0),
        };
        let guarded = InputRequests {
            wda: &fake,
            admission: None,
        };
        for kind in 0..4 {
            assert!(input(kind, &guarded).await);
        }
        assert_eq!(fake.writes.get(), 4);
    }
}
