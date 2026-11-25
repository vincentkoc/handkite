//! Cost admission precedes the mutable rich-response transaction. A cheap
//! hierarchy supplies no input authority or replacement capture evidence.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TargetWorkError {
    Budget,
    Capture,
}

pub(super) async fn target_context<T>(
    wda: &impl WdaRequest,
    session: &str,
    work: impl Future<Output = Result<T, AdapterError>>,
) -> Result<Result<T, TargetWorkError>, AdapterError> {
    let preflight = async {
        let response = {
            let _span = crate::timing::Span::new("target_budget_source");
            wda.request("GET", SOURCE, b"", Duration::from_secs(5))
                .await
                .map_err(|_| TargetWorkError::Capture)?
        };
        let _span = crate::timing::Span::new("target_budget_classify");
        let cost = super::super::outline::cost(&response).map_err(|_| TargetWorkError::Capture)?;
        let admitted = cost.within_visibility_budget();
        crate::timing::outcome("target_budget_admitted", admitted);
        admitted.then_some(()).ok_or(TargetWorkError::Budget)
    }
    .await;
    if let Err(error) = preflight {
        // No settings were read or changed and the inventory future was never
        // polled. Keep this outside the restoration-uncertainty error channel.
        return Ok(Err(error));
    }
    rich_work(wda, session, work).await
}

/// Only the immutable complete snapshot is replayed on this path. The caller
/// proves its original age, identity and both current frames before settings;
/// a miss must use fresh preflight, never fresh enumeration with this count.
pub(super) async fn snapshot_context<T>(
    cost: &super::super::outline::Cost,
    work: impl Future<Output = Result<T, AdapterError>>,
) -> Result<Result<T, TargetWorkError>, AdapterError> {
    if !cost.within_visibility_budget() {
        return Ok(Err(TargetWorkError::Budget));
    }
    crate::timing::outcome("target_budget_snapshot_reuse", true);
    crate::timing::outcome("target_budget_admitted", true);
    // Projection only reads the immutable XML/items and host pixels/OCR. It
    // needs no response-format writes, hence no restoration transaction.
    Ok(work.await.map_err(|_| TargetWorkError::Capture))
}

async fn rich_work<T>(
    wda: &impl WdaRequest,
    session: &str,
    work: impl Future<Output = Result<T, AdapterError>>,
) -> Result<Result<T, TargetWorkError>, AdapterError> {
    rich_context(wda, session, ATTRS, work)
        .await
        .map(|result| result.map_err(|_| TargetWorkError::Capture))
}

impl WdaAdapter {
    pub(super) fn clear_budget_targets(&mut self) {
        self.targets = None;
        self.target_snapshot = None;
        self.target_delta = None;
        self.target_input = None;
        self.target_readback = None;
    }

    pub(super) fn target_work<T>(
        &mut self,
        result: Result<T, TargetWorkError>,
    ) -> Result<T, AdapterError> {
        match result {
            Ok(value) => Ok(value),
            Err(TargetWorkError::Budget) => {
                // An incremental capture may carry an acknowledged tap. End
                // its proof without issuing another full capture next time.
                // Already claimed work still drains through the normal fence.
                self.abandon_pending_effect()?;
                self.clear_budget_targets();
                self.error_diagnostic = Some(AdapterDiagnostic::TargetBudget);
                Err(AdapterError)
            }
            Err(TargetWorkError::Capture) => Err(AdapterError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wda_http::HttpResponse;
    use crate::wda_transport::WdaError;
    use std::cell::{Cell, RefCell};

    struct Fixture {
        xml: String,
        calls: RefCell<Vec<String>>,
        settings: RefCell<Value>,
        fail_source: bool,
        fail_restore: bool,
    }

    impl Fixture {
        fn new(nodes: usize) -> Self {
            Self {
                xml: format!(
                    "<AppiumAUT>{}</AppiumAUT>",
                    "<XCUIElementTypeOther/>".repeat(nodes)
                ),
                calls: RefCell::new(Vec::new()),
                settings: RefCell::new(json!({
                    "shouldUseCompactResponses":true,"elementResponseAttributes":"type"
                })),
                fail_source: false,
                fail_restore: false,
            }
        }
    }

    impl WdaRequest for Fixture {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, WdaError> {
            self.calls.borrow_mut().push(format!("{method} {path}"));
            let result = if path == SOURCE {
                assert_eq!(method, "GET");
                assert!(body.is_empty());
                assert_eq!(timeout, Duration::from_secs(5));
                if self.fail_source {
                    return Err(WdaError::TimedOutAfterSent);
                }
                json!(self.xml)
            } else {
                assert_eq!(path, "/session/session/appium/settings");
                if method == "POST" {
                    let settings =
                        serde_json::from_slice::<Value>(body).unwrap()["settings"].clone();
                    if !(self.fail_restore && settings["shouldUseCompactResponses"] == true) {
                        *self.settings.borrow_mut() = settings;
                    }
                }
                self.settings.borrow().clone()
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session", "value":result})).unwrap(),
            })
        }
    }

    #[tokio::test]
    async fn immutable_projection_failure_is_not_a_restoration_failure() {
        let cost = super::super::super::outline::xml_cost(
            "<AppiumAUT><XCUIElementTypeOther/></AppiumAUT>",
        )
        .unwrap();
        assert_eq!(
            snapshot_context(&cost, async { Err::<(), _>(AdapterError) }).await,
            Ok(Err(TargetWorkError::Capture))
        );
    }

    #[tokio::test]
    async fn oversized_inventory_refuses_before_settings_or_work() {
        for nodes in [351, 1881] {
            let fixture = Fixture::new(nodes);
            let polled = Cell::new(false);
            let result = target_context(&fixture, "session", async {
                polled.set(true);
                Ok(())
            })
            .await;
            assert_eq!(result, Ok(Err(TargetWorkError::Budget)));
            assert!(!polled.get());
            assert_eq!(*fixture.calls.borrow(), [format!("GET {SOURCE}")]);
            assert_eq!(fixture.settings.borrow()["shouldUseCompactResponses"], true);
        }
    }

    #[tokio::test]
    async fn boundary_and_small_webview_keep_the_existing_transaction() {
        for webview in [false, true] {
            let mut fixture = Fixture::new(350);
            if webview {
                fixture.xml =
                    fixture
                        .xml
                        .replacen("XCUIElementTypeOther", "XCUIElementTypeWebView", 1);
            }
            let polled = Cell::new(false);
            assert_eq!(
                target_context(&fixture, "session", async {
                    polled.set(true);
                    assert_eq!(
                        fixture.settings.borrow()["shouldUseCompactResponses"],
                        false
                    );
                    Ok(17)
                })
                .await,
                Ok(Ok(17))
            );
            assert!(polled.get());
            assert_eq!(fixture.settings.borrow()["shouldUseCompactResponses"], true);
            assert_eq!(
                fixture
                    .calls
                    .borrow()
                    .iter()
                    .filter(|p| p.starts_with("POST "))
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn incomplete_or_failed_preflight_is_not_a_restoration_failure() {
        for transport in [false, true] {
            let mut fixture = Fixture::new(1);
            fixture.fail_source = transport;
            if !transport {
                fixture.xml = "<AppiumAUT><XCUIElementTypeOther>".into();
            }
            let result =
                target_context(&fixture, "session", async { panic!("no inventory") }).await;
            assert_eq!(result, Ok(Err::<(), _>(TargetWorkError::Capture)));
            assert_eq!(*fixture.calls.borrow(), [format!("GET {SOURCE}")]);
        }
    }

    #[tokio::test]
    async fn preflight_is_fresh_each_time_and_real_restore_failure_stays_outer() {
        let mut fixture = Fixture::new(1);
        assert_eq!(
            target_context(&fixture, "session", async { Ok(()) }).await,
            Ok(Ok(()))
        );
        fixture.xml = Fixture::new(1881).xml;
        assert_eq!(
            target_context(&fixture, "session", async { panic!("no cached admission") }).await,
            Ok(Err::<(), _>(TargetWorkError::Budget))
        );
        assert_eq!(
            fixture
                .calls
                .borrow()
                .iter()
                .filter(|p| p.as_str() == format!("GET {SOURCE}"))
                .count(),
            2
        );
        fixture.xml = Fixture::new(1).xml;
        fixture.fail_restore = true;
        assert_eq!(
            target_context(&fixture, "session", async { Ok(()) }).await,
            Err(AdapterError)
        );
    }
}
