use super::response::{exchange_reply, metadata, text_result};
use super::{presentation, tool_error};
use crate::wire::Reply;
use handkite_core::controller::LeaseId;
use handkite_core::protocol::{Action, CaptureMode, DeviceKey, ObservedEffect, Revision};
use serde_json::{Value, json};
use std::io;

const OWNER: &str = "handkite-mcp";
pub(super) const CONTROL_TTL_MS: u64 = 120_000;

#[derive(Default)]
pub(super) struct Session {
    pub(super) lease: Option<LeaseId>,
    acquisition_unknown: bool,
    pub(super) presentation: presentation::Cache,
    pub(super) target_inventory: Option<(
        Revision,
        handkite_core::protocol::DeviceState,
        Vec<handkite_core::controller::ElementTarget>,
    )>,
}

impl Session {
    pub(super) fn acquire<F>(&mut self, exchange: &mut F) -> Value
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        if self.acquisition_unknown {
            return tool_error(
                "control acquisition unknown; close this session and wait two minutes before acquiring again",
            );
        }
        if self.lease.is_some() {
            return tool_error("control already held; release before acquiring again");
        }
        match exchange_reply(
            json!({"method": "acquire", "owner": OWNER, "ttl_ms": CONTROL_TTL_MS, "renew_on_success": true}),
            exchange,
        ) {
            Ok(Reply::Acquired { lease }) => {
                self.lease = Some(lease);
                text_result(
                    json!({"control": "acquired", "ttl_ms": CONTROL_TTL_MS, "renewal": "verified_action"}),
                )
            }
            Ok(Reply::Error { code }) => service_error(&code),
            _ => {
                // The daemon may own a lease whose reply was lost. Never reacquire it automatically.
                self.acquisition_unknown = true;
                tool_error(
                    "control acquisition unknown; close this session and wait two minutes before acquiring again",
                )
            }
        }
    }

    pub(super) fn release<F>(&mut self, exchange: &mut F) -> Value
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        let Some(lease) = self.lease else {
            return tool_error("no control held by this session");
        };
        match exchange_reply(
            json!({"method": "release", "owner": OWNER, "lease": lease}),
            exchange,
        ) {
            Ok(Reply::Released) => {
                self.lease = None;
                text_result(json!({"control": "released"}))
            }
            Ok(Reply::Error { code }) => {
                if code == "InvalidLease" {
                    self.lease = None;
                }
                service_error(&code)
            }
            _ => tool_error("control release unknown; expiry still bounds control"),
        }
    }

    pub(super) fn cleanup<F>(&mut self, exchange: &mut F)
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        if self.lease.is_some() {
            let _ = self.release(exchange);
        }
    }

    pub(super) fn act<F>(
        &mut self,
        device: &DeviceKey,
        revision: Revision,
        action: Action,
        include_observation: bool,
        options: presentation::Options,
        exchange: &mut F,
    ) -> Value
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        let Some(lease) = self.lease else {
            return tool_error("explicit control acquisition required");
        };
        let kind = action.kind();
        let navigation = matches!(action, Action::Home | Action::LaunchApp { .. });
        let mut request = json!({"method": if navigation { "navigate" } else { "act" }, "owner": OWNER, "lease": lease,
            "request": {"device": device, "expected_revision": revision, "action": action}});
        if !navigation && options.capture != CaptureMode::Paired {
            request["capture"] = json!(options.capture);
        }
        let response = exchange_reply(request, exchange);
        match response {
            Ok(Reply::Navigated {
                receipt,
                effect,
                after,
                effect_updates,
            }) if navigation
                && receipt.action_kind == kind
                && effect != ObservedEffect::LockConfirmed =>
            {
                self.presentation.clear();
                if after.as_ref().is_some_and(|after| &after.device != device) {
                    return uncertain_result(kind);
                }
                let mut payload = json!({"receipt":receipt,"effect":effect,
                    "after":after.as_ref().map(metadata)});
                if !effect_updates.is_empty() {
                    payload["effect_updates"] = json!(effect_updates);
                }
                let mut content = Vec::new();
                if let Some(after) = after {
                    let observed = super::response::navigation_content(
                        after,
                        "navigation",
                        include_observation && options.image != presentation::ImageMode::Omit,
                    );
                    payload["observation"] =
                        serde_json::from_str::<Value>(observed[0]["text"].as_str().unwrap())
                            .unwrap();
                    content.extend(observed.into_iter().skip(1));
                }
                let mut result = text_result(payload.clone());
                result["structuredContent"] = payload;
                result["content"].as_array_mut().unwrap().extend(content);
                if effect == ObservedEffect::Unconfirmed || receipt.accepted != Some(true) {
                    result["isError"] = json!(true);
                }
                result
            }
            Ok(Reply::ViewChanged { changed }) if changed.any() => {
                let mut result = service_error("ViewChanged");
                result["structuredContent"] = json!({"code":"ViewChanged","changed":changed});
                result
            }
            Ok(Reply::Acted {
                receipt,
                after,
                after_image,
                native_lock: None,
                effect,
                effect_updates,
            }) if receipt.action_kind == kind && effect != ObservedEffect::LockConfirmed => {
                if after.is_some() && after_image.is_some()
                    || after_image.as_ref().is_some_and(|observation| {
                        &observation.device != device || options.capture != CaptureMode::ImageOnly
                    })
                    || after
                        .as_ref()
                        .is_some_and(|observation| &observation.device != device)
                {
                    return uncertain_result(kind);
                }
                let mut result = text_result(json!({"receipt": receipt, "effect": effect,
                    "after": after.as_ref().map(metadata)}));
                if let Some(ref image) = after_image {
                    result["content"][0]["text"] = json!(
                        json!({"receipt":receipt,"effect":effect,
                        "after":null,"after_image":super::response::image_metadata(image)})
                        .to_string()
                    );
                    self.presentation.clear();
                }
                if effect == ObservedEffect::Unconfirmed || receipt.accepted != Some(true) {
                    result["isError"] = json!(true);
                }
                if include_observation && let Some(observation) = after {
                    let content = self.presentation.content(observation, options);
                    if options.tree_format == presentation::TreeFormat::Elements {
                        let mut payload: Value =
                            serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                                .unwrap();
                        payload["observation"] =
                            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
                        result["structuredContent"] = payload.clone();
                        result["content"][0]["text"] = json!(payload.to_string());
                        result["content"]
                            .as_array_mut()
                            .unwrap()
                            .extend(content.into_iter().skip(1));
                    } else {
                        result["content"]
                            .as_array_mut()
                            .expect("text result has a content array")
                            .extend(content);
                    }
                }
                if include_observation && let Some(observation) = after_image {
                    result["content"]
                        .as_array_mut()
                        .expect("text result has content")
                        .extend(super::response::image_content(observation));
                }
                if !effect_updates.is_empty() {
                    let mut payload: Value =
                        serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                            .unwrap();
                    payload["effect_updates"] = json!(effect_updates);
                    result["content"][0]["text"] = json!(payload.to_string());
                    result["structuredContent"] = payload;
                }
                result
            }
            Ok(Reply::Error { code }) => {
                if code == "InvalidLease" {
                    self.lease = None;
                }
                // Only explicit pre-input refusals prove that input was not sent.
                // Identity checks and response-size limits can also fail after input.
                if code == "AdapterPreActionCapture" {
                    let mut result = text_result(json!({
                        "receipt": {"accepted": false, "action_kind": kind},
                        "effect": "unconfirmed", "after": null, "delivery": "not_sent"
                    }));
                    result["isError"] = json!(true);
                    super::response::with_service_code(result, &code)
                } else if navigation && code == "StaleObservation" {
                    service_error(&code)
                } else if matches!(
                    code.as_str(),
                    "InvalidLease" | "StaleRevision" | "ViewChanged" | "LockedAction"
                ) {
                    service_error(&code)
                } else {
                    uncertain_service_result(kind, &code)
                }
            }
            _ => uncertain_result(kind),
        }
    }
}

fn uncertain_result(kind: &str) -> Value {
    let mut result = text_result(json!({"receipt": {"accepted": null, "action_kind": kind},
        "effect": "unconfirmed", "after": null, "delivery": "unknown"}));
    result["isError"] = json!(true);
    result
}

fn uncertain_service_result(kind: &str, code: &str) -> Value {
    // Keep delivery unknown while exposing only the controller's fixed categories.
    // Never forward arbitrary service text, which may contain private content.
    super::response::with_service_code(uncertain_result(kind), code)
}

fn service_error(code: &str) -> Value {
    // Never forward service detail that could contain a token or private screen content.
    super::response::with_service_code(
        tool_error(match code {
            "Busy" => "Busy: another session holds control",
            "InvalidLease" => "InvalidLease: explicit control acquisition required",
            "StaleRevision" => "StaleRevision: obtain a fresh observation before another action",
            "StaleObservation" => {
                "StaleObservation: app context changed before navigation; obtain a fresh observation"
            }
            "ViewChanged" => {
                "ViewChanged: screen context changed before input; obtain a fresh observation"
            }
            "DeviceMismatch" => "DeviceMismatch: pinned identity verification failed",
            "LockedAction" => "LockedAction: action requires an unlocked phone",
            _ => "device service rejected the request",
        }),
        code,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_action_reply(reply: io::Result<Vec<u8>>) -> Value {
        let lease = LeaseId::try_from("1".repeat(32)).unwrap();
        let mut session = Session {
            lease: Some(lease),
            ..Session::default()
        };
        let mut reply = Some(reply);
        let mut calls = 0;
        let result = session.act(
            &DeviceKey::new("fixture-device").unwrap(),
            Revision(1),
            Action::TextCurrent {
                value: "fixture input".into(),
            },
            true,
            presentation::Options::default(),
            &mut |request| {
                calls += 1;
                let request: Value = serde_json::from_slice(request).unwrap();
                assert_eq!(request["method"], "act");
                assert_eq!(request["request"]["action"]["type"], "text_current");
                reply.take().expect("input must not be retried")
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(session.lease, Some(lease));
        assert!(!result.to_string().contains("fixture input"));
        result
    }

    fn text_error_reply(code: &str) -> Value {
        text_action_reply(Ok(
            serde_json::to_vec(&Reply::Error { code: code.into() }).unwrap()
        ))
    }

    #[test]
    fn preaction_capture_reports_input_not_sent_without_retry() {
        let result = text_error_reply("AdapterPreActionCapture");
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["code"],
            "AdapterPreActionCapture"
        );
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["receipt"]["accepted"], false);
        assert_eq!(payload["receipt"]["action_kind"], "text");
        assert_eq!(payload["delivery"], "not_sent");
        assert_eq!(payload["effect"], "unconfirmed");
        assert_eq!(payload["after"], Value::Null);
    }

    #[test]
    fn failed_or_malformed_action_exchange_keeps_delivery_unknown() {
        for reply in [
            Err(io::Error::other("private transport detail")),
            Ok(b"not-json".to_vec()),
        ] {
            let result = text_action_reply(reply);
            assert_eq!(result["isError"], true);
            let payload: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(payload["receipt"]["accepted"], Value::Null);
            assert_eq!(payload["delivery"], "unknown");
            assert_eq!(payload["effect"], "unconfirmed");
            assert_eq!(payload["after"], Value::Null);
            assert!(result.get("structuredContent").is_none());
            assert!(!result.to_string().contains("private"));
        }
    }

    #[test]
    fn service_errors_preserve_fixed_codes_and_redact_unknown_details() {
        for (code, message) in [
            ("Busy", "Busy: another session holds control"),
            (
                "InvalidLease",
                "InvalidLease: explicit control acquisition required",
            ),
            (
                "StaleRevision",
                "StaleRevision: obtain a fresh observation before another action",
            ),
            (
                "ViewChanged",
                "ViewChanged: screen context changed before input; obtain a fresh observation",
            ),
            (
                "DeviceMismatch",
                "DeviceMismatch: pinned identity verification failed",
            ),
            (
                "LockedAction",
                "LockedAction: action requires an unlocked phone",
            ),
        ] {
            let result = service_error(code);
            assert_eq!(result["isError"], true);
            assert_eq!(result["structuredContent"]["code"], code);
            assert_eq!(result["content"][0]["text"], message);
        }
        for code in ["private screen content", "Busy: private detail", ""] {
            let result = service_error(code);
            assert_eq!(result["isError"], true);
            assert_eq!(
                result["content"][0]["text"],
                "device service rejected the request"
            );
            assert!(result.get("structuredContent").is_none());
            assert!(!result.to_string().contains("private"));
        }
    }

    #[test]
    fn uncertain_service_errors_expose_only_fixed_diagnostic_codes() {
        for code in ["StaleObservation", "InvalidObservationWindow", "Adapter"]
            .into_iter()
            .chain(handkite_core::controller::AdapterDiagnostic::ALL.map(|value| value.code()))
            .filter(|code| *code != "AdapterPreActionCapture")
        {
            let reply = text_error_reply(code);
            assert_eq!(reply["structuredContent"]["code"], code);
            assert_eq!(reply["isError"], true);
            let payload: Value =
                serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(payload["receipt"]["accepted"], Value::Null);
            assert_eq!(payload["after"], Value::Null);
            assert_eq!(payload["effect"], "unconfirmed");
            assert_eq!(payload["delivery"], "unknown");
        }
        for code in [
            "private screen content",
            "Adapter: private detail",
            "AdapterInputConnect: private detail",
            "AdapterPreActionCapture: private detail",
            "adapterpreactioncapture",
            "AdapterAt(InputConnect)",
            "",
        ] {
            let reply = text_error_reply(code);
            assert!(reply.get("structuredContent").is_none());
            assert!(!reply.to_string().contains("private"));
            let payload: Value =
                serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(payload["receipt"]["accepted"], Value::Null);
            assert_eq!(payload["delivery"], "unknown");
        }
    }
}
