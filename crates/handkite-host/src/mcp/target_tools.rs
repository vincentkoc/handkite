use super::{response, session::Session, tool_error};
use crate::wire::Reply;
use handkite_core::{
    controller::ElementAction,
    protocol::{DeviceKey, Revision},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetResponse {
    #[serde(default = "omit_image")]
    image: super::presentation::ImageMode,
}
fn omit_image() -> super::presentation::ImageMode {
    super::presentation::ImageMode::Omit
}
impl Default for TargetResponse {
    fn default() -> Self {
        Self {
            image: omit_image(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tap {
    expected_revision: Revision,
    target_id: String,
    #[serde(default)]
    response: TargetResponse,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    expected_revision: Revision,
    target_id: String,
    #[serde(default)]
    response: TargetResponse,
    value: String,
}
fn parse(
    text: bool,
    raw: &str,
) -> Result<(Revision, ElementAction, super::presentation::ImageMode), ()> {
    let (revision, action, image) = if text {
        let args: Text = serde_json::from_str(raw).map_err(|_| ())?;
        (
            args.expected_revision,
            ElementAction::Text {
                target_id: args.target_id,
                value: args.value,
            },
            args.response.image,
        )
    } else {
        let args: Tap = serde_json::from_str(raw).map_err(|_| ())?;
        (
            args.expected_revision,
            ElementAction::Tap {
                target_id: args.target_id,
            },
            args.response.image,
        )
    };
    action
        .valid()
        .then_some((revision, action, image))
        .ok_or(())
}
pub(super) fn valid_raw(text: bool, raw: &str) -> bool {
    parse(text, raw).is_ok()
}
pub(super) fn schema(text: bool) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false,"properties":{"expected_revision":{"type":"integer","minimum":0},"target_id":{"type":"string","pattern":"^t_[0-9a-f]{32}$"}},"required":["expected_revision","target_id"]});
    schema["properties"]["response"] = json!({"type":"object","additionalProperties":false,"properties":{"image":{"type":"string","enum":["omit","changed","always"],"default":"omit"}}});
    if text {
        schema["properties"]["value"] = json!({"type":"string","minLength":1});
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("value"));
    }
    schema
}
impl Session {
    pub(super) fn act_element<F>(
        &mut self,
        text: bool,
        args: Value,
        device: &DeviceKey,
        exchange: &mut F,
    ) -> Value
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        let Some(lease) = self.lease else {
            return tool_error("explicit control acquisition required");
        };
        let Ok((revision, action, image)) = parse(text, &args.to_string()) else {
            return tool_error("invalid native target arguments");
        };
        let kind = action.kind();
        let acted_target = action.target_id().to_owned();
        match response::exchange_reply(
            json!({"method":"act_element","owner":"handkite-mcp","lease":lease,"device":device,"expected_revision":revision,"action":action}),
            exchange,
        ) {
            Ok(Reply::ElementActed {
                receipt,
                after_image,
                effect,
                next,
                targets,
                effect_updates,
            }) if receipt.action_kind == kind
                && next.as_ref().is_none_or(|next| &next.device == device)
                && after_image
                    .as_ref()
                    .is_none_or(|image| &image.device == device) =>
            {
                let mut result = response::text_result(
                    json!({"receipt":receipt,"effect":effect,"effect_updates":effect_updates,"revision":next.as_ref().map(|next|next.revision),"target_only":true,"targets":targets,
                        "target_delta":{"scope":"acted_element","base_revision":revision,"provisional":effect == handkite_core::protocol::ObservedEffect::Pending,"consumed":[acted_target],"removed":[],"added":[]}}),
                );
                let empty_targets = targets.is_empty();
                if empty_targets {
                    let mut payload: Value =
                        serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                            .unwrap();
                    payload["target_actionable"] = json!(false);
                    payload["target_reason"] = json!(if effect
                        == handkite_core::protocol::ObservedEffect::Pending
                    {
                        "verification_pending"
                    } else {
                        "no_stable_native_targets"
                    });
                    result["content"][0]["text"] = json!(payload.to_string());
                }
                if let Some(observation) = next {
                    if observation.accessibility_tree.is_empty() {
                        self.presentation.clear();
                        let payload = json!({
                            "receipt":receipt,"effect":effect,"effect_updates":effect_updates,"revision":observation.revision,
                            "target_only":true,"target_actionable":!empty_targets,"targets":targets,
                            "target_delta":{"scope":"acted_element","base_revision":revision,
                                "removed":[acted_target],"added":targets},
                            "observation":{"metadata":response::metadata(&observation),
                                "capture_kind":"image_only","tree":{"included":false,"reason":"on_request"}},
                        });
                        let content = response::finish_observation_content(
                            payload.clone(),
                            observation.screenshot_format,
                            observation.screenshot_base64,
                            true,
                        );
                        result = json!({"content":content,"structuredContent":payload});
                    } else {
                        let options = super::presentation::Options {
                            tree_format: super::presentation::TreeFormat::Elements,
                            since_revision: Some(revision),
                            image: if targets.is_empty() {
                                super::presentation::ImageMode::Always
                            } else {
                                image
                            },
                            ..Default::default()
                        };
                        let content = self.presentation.content(observation, options);
                        let mut payload: Value =
                            serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
                                .unwrap();
                        payload["observation"] =
                            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
                        payload["target_actionable"] = json!(!empty_targets);
                        if empty_targets {
                            payload["target_reason"] = json!("no_stable_native_targets");
                        }
                        payload["observation"]["metadata"]["target_only"] = json!(true);
                        result["structuredContent"] = payload.clone();
                        result["content"][0]["text"] = json!(payload.to_string());
                        result["content"]
                            .as_array_mut()
                            .unwrap()
                            .extend(content.into_iter().skip(1));
                    }
                } else if (empty_targets || image != super::presentation::ImageMode::Omit)
                    && let Some(image) = after_image
                {
                    result["content"]
                        .as_array_mut()
                        .unwrap()
                        .extend(response::image_content(image));
                }
                // A native click acknowledgement is not evidence of an effect.
                if receipt.accepted != Some(true)
                    || effect == handkite_core::protocol::ObservedEffect::Unconfirmed
                {
                    result["isError"] = json!(true);
                }
                if result.get("structuredContent").is_none() {
                    result["structuredContent"] = serde_json::from_str::<Value>(
                        result["content"][0]["text"].as_str().unwrap(),
                    )
                    .unwrap();
                }
                result
            }
            Ok(Reply::Error { code }) => {
                if code == "InvalidLease" {
                    self.lease = None;
                }
                // Only these stages end before entering any input-dispatch future.
                let not_sent = matches!(
                    code.as_str(),
                    "AdapterTargetPrepare"
                        | "AdapterTargetRevalidation"
                        | "StaleObservation"
                        | "StaleRevision"
                        | "InvalidLease"
                        | "LockedAction"
                        | "InvalidObservationWindow"
                );
                response::with_service_code(target_error(kind, not_sent), &code)
            }
            _ => target_error(kind, false),
        }
    }
}

fn target_error(kind: &str, not_sent: bool) -> Value {
    let mut result = response::text_result(json!({
        "receipt": {"accepted": if not_sent { Some(false) } else { None }, "action_kind": kind},
        "effect": "unconfirmed",
        "after": null,
        "delivery": if not_sent { "not_sent" } else { "unknown" }
    }));
    result["isError"] = json!(true);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_reply(text: bool, reply: io::Result<Vec<u8>>) -> Value {
        let lease = handkite_core::controller::LeaseId::try_from("1".repeat(32)).unwrap();
        let mut session = Session::default();
        session.lease = Some(lease);
        let mut args = json!({
            "expected_revision":7,
            "target_id":"t_0123456789abcdef0123456789abcdef"
        });
        if text {
            args["value"] = json!("private fixture input");
        }
        let mut reply = Some(reply);
        let mut calls = 0;
        let result = session.act_element(
            text,
            args,
            &DeviceKey::new("fixture").unwrap(),
            &mut |bytes| {
                calls += 1;
                let request: Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(request["method"], "act_element");
                reply.take().expect("target actions must not retry")
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(session.lease, Some(lease));
        assert!(!result.to_string().contains("private fixture input"));
        result
    }

    fn error_reply(text: bool, code: &str) -> Value {
        action_reply(
            text,
            Ok(serde_json::to_vec(&Reply::Error { code: code.into() }).unwrap()),
        )
    }

    fn payload(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn pending_ack_has_provisional_delta_and_no_input_authority() {
        use handkite_core::protocol::{
            ImageFormat, ObservationWindow, ObservedEffect, TransportReceipt,
        };
        let reply = Reply::ElementActed {
            effect_updates: vec![handkite_core::controller::TargetEffect {
                token: 1,
                target_id: "t_0123456789abcdef0123456789abcdef".into(),
                effect: ObservedEffect::Pending,
                region: [0., 0., 1., 1.],
            }],
            receipt: TransportReceipt {
                accepted: Some(true),
                action_kind: "tap_element".into(),
            },
            effect: ObservedEffect::Pending,
            next: None,
            targets: vec![],
            after_image: Some(crate::wire::WireImageObservation {
                device: DeviceKey::new("fixture").unwrap(),
                window: ObservationWindow {
                    started_unix_ms: 1,
                    finished_unix_ms: 2,
                },
                screenshot_format: ImageFormat::Png,
                screenshot_base64: "AA==".into(),
                effect_updates: vec![],
            }),
        };
        let result = action_reply(false, Ok(serde_json::to_vec(&reply).unwrap()));
        let p = payload(&result);
        assert_ne!(result.get("isError"), Some(&json!(true)));
        assert_eq!(p["effect"], "pending");
        assert_eq!(p["target_delta"]["provisional"], true);
        assert_eq!(p["target_delta"]["removed"], json!([]));
        assert!(p["revision"].is_null());
        assert_eq!(p["targets"], json!([]));
        assert_eq!(p["target_reason"], "verification_pending");
        assert_eq!(
            result["content"].as_array().unwrap().last().unwrap()["type"],
            "image"
        );
    }

    #[test]
    fn scoped_image_reply_is_a_partial_target_delta_not_a_full_tree_diff() {
        use handkite_core::protocol::{
            DeviceState, ImageFormat, LockState, ObservationWindow, ObservedEffect,
            TransportReceipt,
        };
        let previous = handkite_core::controller::TargetEffect {
            token: 7,
            target_id: "t_abcdef0123456789abcdef0123456789".into(),
            effect: ObservedEffect::Changed,
            region: [1., 2., 3., 4.],
        };
        let reply = Reply::ElementActed {
            effect_updates: vec![previous.clone()],
            receipt: TransportReceipt {
                accepted: Some(true),
                action_kind: "tap_element".into(),
            },
            effect: ObservedEffect::Changed,
            next: Some(crate::wire::WireObservation {
                effect_updates: Vec::new(),
                device: DeviceKey::new("fixture").unwrap(),
                revision: Revision(8),
                window: ObservationWindow {
                    started_unix_ms: 1,
                    finished_unix_ms: 2,
                },
                state: DeviceState {
                    lock: LockState::Unlocked,
                    wda_ready: true,
                    foreground_app: Some("example.app".into()),
                },
                accessibility_tree: String::new(),
                screenshot_format: ImageFormat::Png,
                screenshot_base64: "AA==".into(),
            }),
            targets: vec![],
            after_image: None,
        };
        let result = action_reply(false, Ok(serde_json::to_vec(&reply).unwrap()));
        let body = payload(&result);
        assert_eq!(body["effect_updates"], json!([previous]));
        assert_eq!(body["target_delta"]["scope"], "acted_element");
        assert_eq!(body["target_delta"]["base_revision"], 7);
        assert_eq!(body["revision"], 8);
        assert_eq!(body["observation"]["capture_kind"], "image_only");
        assert_eq!(body["observation"]["tree"]["included"], false);
        assert!(body.get("elements_delta").is_none());
        assert!(body["observation"].get("elements_delta").is_none());
        assert_eq!(result["content"][1]["type"], "image");
    }

    #[test]
    fn exact_preinput_target_diagnostics_report_not_sent_without_retry() {
        for text in [false, true] {
            for code in [
                "AdapterTargetPrepare",
                "AdapterTargetRevalidation",
                "StaleObservation",
                "StaleRevision",
                "LockedAction",
                "InvalidObservationWindow",
            ] {
                let result = error_reply(text, code);
                let body = payload(&result);
                assert_eq!(result["isError"], true);
                assert_eq!(result["structuredContent"]["code"], code);
                assert_eq!(body["receipt"]["accepted"], false);
                assert_eq!(
                    body["receipt"]["action_kind"],
                    if text { "text_element" } else { "tap_element" }
                );
                assert_eq!(body["delivery"], "not_sent");
                assert_eq!(body["effect"], "unconfirmed");
                assert!(body.get("after").is_some_and(Value::is_null));
            }
        }
    }

    #[test]
    fn other_target_diagnostics_keep_delivery_unknown_and_redact_details() {
        for text in [false, true] {
            for code in handkite_core::controller::AdapterDiagnostic::ALL
                .iter()
                .map(|diagnostic| diagnostic.code())
                .filter(|code| {
                    !matches!(
                        *code,
                        "AdapterTargetPrepare"
                            | "AdapterTargetRevalidation"
                            | "StaleObservation"
                            | "StaleRevision"
                            | "InvalidLease"
                            | "LockedAction"
                            | "InvalidObservationWindow"
                    )
                })
                .chain([
                    "Adapter",
                    "DeviceMismatch",
                    "AdapterTargetPrepare private detail",
                    "AdapterTargetRevalidationAfterSend",
                ])
            {
                let result = error_reply(text, code);
                let body = payload(&result);
                assert_eq!(result["isError"], true);
                assert!(body["receipt"].get("accepted").is_some_and(Value::is_null));
                assert_eq!(
                    body["receipt"]["action_kind"],
                    if text { "text_element" } else { "tap_element" }
                );
                assert_eq!(body["delivery"], "unknown");
                assert_eq!(body["effect"], "unconfirmed");
                assert!(body.get("after").is_some_and(Value::is_null));
                if matches!(
                    code,
                    "AdapterTargetPrepare private detail" | "AdapterTargetRevalidationAfterSend"
                ) {
                    assert!(!result.to_string().contains(code));
                } else {
                    assert_eq!(result["structuredContent"]["code"], code);
                }
            }
        }
    }

    #[test]
    fn missing_or_malformed_target_reply_keeps_delivery_unknown() {
        for reply in [
            Err(io::Error::other("private transport detail")),
            Ok(b"malformed private reply".to_vec()),
            Ok(br#"{"type":"error"}"#.to_vec()),
            Ok(serde_json::to_vec(&json!({
                "type":"element_acted",
                "receipt":{"accepted":true,"action_kind":"other"},
                "effect":"changed","after_image":null,"next":null,"targets":[]
            }))
            .unwrap()),
        ] {
            let result = action_reply(true, reply);
            let body = payload(&result);
            assert_eq!(result["isError"], true);
            assert!(body["receipt"].get("accepted").is_some_and(Value::is_null));
            assert_eq!(body["delivery"], "unknown");
            assert_eq!(body["effect"], "unconfirmed");
            assert!(body.get("after").is_some_and(Value::is_null));
            assert!(!result.to_string().contains("private"));
        }
    }

    #[test]
    fn invalid_lease_allows_only_explicit_reacquisition_without_replay() {
        use handkite_core::controller::LeaseId;
        let mut session = Session::default();
        session.lease = Some(LeaseId::try_from(format!("{:032x}", 1)).unwrap());
        let device = DeviceKey::new("phone").unwrap();
        let mut calls = Vec::new();
        let mut exchange = |bytes: &[u8]| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            calls.push(request["method"].as_str().unwrap().to_owned());
            Ok(serde_json::to_vec(&Reply::Error {
                code: "InvalidLease".into(),
            })
            .unwrap())
        };
        let result = session.act_element(
            false,
            json!({"expected_revision":7,"target_id":"t_0123456789abcdef0123456789abcdef"}),
            &device,
            &mut exchange,
        );
        assert_eq!(result["isError"], true);
        assert!(session.lease.is_none());
        assert_eq!(calls, vec!["act_element"]);
        let mut reacquire = |bytes: &[u8]| {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            calls.push(request["method"].as_str().unwrap().to_owned());
            Ok(serde_json::to_vec(&Reply::Acquired {
                lease: LeaseId::try_from(format!("{:032x}", 2)).unwrap(),
            })
            .unwrap())
        };
        session.acquire(&mut reacquire);
        assert_eq!(
            session.lease,
            Some(LeaseId::try_from(format!("{:032x}", 2)).unwrap())
        );
        assert_eq!(calls, vec!["act_element", "acquire"]);
    }

    #[test]
    fn target_tools_require_opaque_ids_and_reject_presentation_or_control_text() {
        assert!(valid_raw(
            false,
            r#"{"expected_revision":1,"target_id":"t_0123456789abcdef0123456789abcdef"}"#
        ));
        assert!(!valid_raw(
            false,
            r#"{"expected_revision":1,"target_id":"e1"}"#
        ));
        assert!(!valid_raw(
            false,
            r#"{"expected_revision":1,"target_id":"t_0123456789abcdef0123456789abcdef","at":{"x":1,"y":1}}"#
        ));
        for value in ["", "\n", "\u{2028}", "\u{2029}", "\u{f700}"] {
            assert!(!valid_raw(true,&json!({"expected_revision":1,"target_id":"t_0123456789abcdef0123456789abcdef","value":value}).to_string()));
        }
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    #[test]
    fn acknowledged_native_baseline_requests_region_capture_and_delta_only_reply() {
        let device = DeviceKey::new("phone").unwrap();
        let mut session = Session::default();
        let target = |id: &str| json!({"target_id":id,"source":"native","kind":"XCUIElementTypeButton","label":"Choose","can_type":false,"text_only":false,"rect":[1.,2.,3.,4.],"value":null,"value_truncated":false});
        let mut calls = 0;
        for revision in [1, 2] {
            let options: super::super::presentation::Options =
                serde_json::from_value(if revision == 1 {
                    json!({"targets":true})
                } else {
                    json!({"targets":true,"tree_format":"elements","since_revision":1})
                })
                .unwrap();
            let mut exchange = |bytes: &[u8]| {
                calls += 1;
                let request: Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(request["method"], "observe_targets");
                assert_eq!(request.get("since_revision").is_some(), revision == 2);
                let xml = format!(
                    "<AppiumAUT source=\"native_regions\"><XCUIElementTypeApplication visible=\"true\" x=\"0\" y=\"0\" width=\"20\" height=\"20\"><XCUIElementTypeButton visible=\"true\" enabled=\"true\" label=\"Choose\" source=\"native\" target_id=\"t_0123456789abcdef0123456789abcdef\" x=\"1\" y=\"2\" width=\"3\" height=\"4\"/></XCUIElementTypeApplication></AppiumAUT>"
                );
                Ok(serde_json::to_vec(&json!({"type":"observed_targets","observation":{"device":"phone","revision":revision,"window":{"started_unix_ms":1,"finished_unix_ms":2},"state":{"lock":"unlocked","wda_ready":true,"foreground_app":"example.app"},"accessibility_tree":xml,"screenshot_format":"png","screenshot_base64":"AA==","effect_updates":[{"token":9,"target_id":"t_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","effect":"changed","region":[1.,2.,3.,4.]}]},"targets":[target("t_0123456789abcdef0123456789abcdef")]})).unwrap())
            };
            let result = super::super::tool_result(
                super::super::catalog::Tool::Observe,
                options,
                &device,
                &mut session,
                &mut exchange,
            );
            let p = &result["structuredContent"];
            assert_eq!(p["tree_scope"], "native_regions");
            assert_eq!(p["metadata"]["effect_updates"][0]["effect"], "changed");
            if revision == 2 {
                assert!(p.get("elements").is_none());
                assert!(p.get("targets").is_none());
                assert_eq!(p["targets_delta"]["base_revision"], 1);
                assert_eq!(p["elements_delta"]["base_revision"], 1);
            }
        }
        assert_eq!(calls, 2);
    }

    #[test]
    fn empty_target_observation_forces_image_fallback() {
        let options: super::super::presentation::Options =
            serde_json::from_value(json!({"targets":true})).unwrap();
        let mut session = Session::default();
        let device = DeviceKey::new("phone").unwrap();
        let mut calls = 0;
        let mut exchange = |bytes: &[u8]| {
            calls += 1;
            assert_eq!(
                serde_json::from_slice::<Value>(bytes).unwrap(),
                json!({"method":"observe_targets","device":"phone"})
            );
            Ok(serde_json::to_vec(&json!({"type":"observed_targets","observation":{"device":"phone","revision":7,"window":{"started_unix_ms":1,"finished_unix_ms":2},"state":{"lock":"unlocked","wda_ready":true,"foreground_app":"example.app"},"accessibility_tree":"<AppiumAUT><XCUIElementTypeApplication x=\"0\" y=\"0\" width=\"400\" height=\"800\"/></AppiumAUT>","screenshot_format":"png","screenshot_base64":"AA=="},"targets":[]})).unwrap())
        };
        let result = super::super::tool_result(
            super::super::catalog::Tool::Observe,
            options,
            &device,
            &mut session,
            &mut exchange,
        );
        assert_eq!(calls, 1);
        assert_eq!(result["structuredContent"]["target_only"], true);
        assert_eq!(result["structuredContent"]["metadata"]["target_only"], true);
        assert_eq!(result["structuredContent"]["revision"], 7);
        assert_eq!(result["content"].as_array().unwrap().len(), 2);
        assert_eq!(result["structuredContent"]["target_actionable"], false);
    }
}

#[cfg(test)]
mod dispatcher_integration {
    use super::*;
    use crate::wda_action::WdaRequest;
    use crate::wda_http::HttpResponse;
    use crate::wda_transport::WdaError;
    use std::{cell::Cell, time::Duration};
    struct Native {
        sends: Cell<usize>,
    }
    impl WdaRequest for Native {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, WdaError> {
            let value = if method == "POST" {
                assert_eq!(path, "/session/session/wda/keys");
                let body: Value = serde_json::from_slice(body).unwrap();
                assert_eq!(body["value"], json!(["hello"]));
                assert!(body["frequency"].as_u64().is_some());
                self.sends.set(self.sends.get() + 1);
                Value::Null
            } else if path.ends_with("/element/active") {
                json!({"ELEMENT":"field"})
            } else if path.ends_with("/wda/activeAppInfo") {
                json!({"bundleId":"example.editor"})
            } else {
                assert!(path.ends_with("/wda/locked"));
                json!(false)
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&json!({"sessionId":"session","value":value})).unwrap(),
            })
        }
    }
    #[tokio::test]
    async fn native_text_ack_keeps_element_kind_through_mcp_without_second_send() {
        let native = Native {
            sends: Cell::new(0),
        };
        let action = ElementAction::Text {
            target_id: "t_0123456789abcdef0123456789abcdef".into(),
            value: "hello".into(),
        };
        let receipt = crate::wda_adapter::test_dispatch_native(
            &native,
            "session",
            "example.editor",
            "field",
            &action,
        )
        .await
        .unwrap();
        assert_eq!(receipt.action_kind, "text_element");
        let mut session = Session::default();
        session.lease = Some(handkite_core::controller::LeaseId::try_from("1".repeat(32)).unwrap());
        let mut calls = 0;
        let mut exchange = |bytes: &[u8]| {
            calls += 1;
            assert_eq!(
                serde_json::from_slice::<Value>(bytes).unwrap()["method"],
                "act_element"
            );
            Ok(serde_json::to_vec(&Reply::ElementActed {
                effect_updates: Vec::new(),
                receipt: receipt.clone(),
                effect: handkite_core::protocol::ObservedEffect::Changed,
                next: None,
                targets: vec![],
                after_image: None,
            })
            .unwrap())
        };
        let result = session.act_element(
            true,
            json!({"expected_revision":7,"target_id":action.target_id(),"value":"hello"}),
            &DeviceKey::new("phone").unwrap(),
            &mut exchange,
        );
        assert_ne!(result.get("isError"), Some(&json!(true)));
        assert_eq!(calls, 1);
        assert_eq!(native.sends.get(), 1);
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["receipt"]["action_kind"], "text_element");
    }
}
