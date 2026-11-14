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
    lease: Option<LeaseId>,
    acquisition_unknown: bool,
    pub(super) presentation: presentation::Cache,
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
            json!({"method": "acquire", "owner": OWNER, "ttl_ms": CONTROL_TTL_MS}),
            exchange,
        ) {
            Ok(Reply::Acquired { lease }) => {
                self.lease = Some(lease);
                text_result(json!({"control": "acquired", "ttl_ms": CONTROL_TTL_MS}))
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
        let mut request = json!({"method": "act", "owner": OWNER, "lease": lease,
            "request": {"device": device, "expected_revision": revision, "action": action}});
        if options.capture != CaptureMode::Paired {
            request["capture"] = json!(options.capture);
        }
        let response = exchange_reply(request, exchange);
        match response {
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
                    result["content"]
                        .as_array_mut()
                        .expect("text result has a content array")
                        .extend(self.presentation.content(observation, options));
                }
                if include_observation && let Some(observation) = after_image {
                    result["content"]
                        .as_array_mut()
                        .expect("text result has content")
                        .extend(super::response::image_content(observation));
                }
                result
            }
            Ok(Reply::Error { code }) => {
                if code == "InvalidLease" {
                    self.lease = None;
                }
                // Only these controller failures prove that execute was never called.
                // Identity checks and response-size limits can also fail after input.
                if matches!(
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
        {
            let reply = uncertain_service_result("tap", code);
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
            "AdapterAt(InputConnect)",
            "",
        ] {
            let reply = uncertain_service_result("tap", code);
            assert!(reply.get("structuredContent").is_none());
            assert!(!reply.to_string().contains("private"));
        }
    }
}
