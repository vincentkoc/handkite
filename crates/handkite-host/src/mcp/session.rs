use super::response::{exchange_reply, metadata, observation_content, text_result};
use super::tool_error;
use crate::wire::Reply;
use handkite_core::controller::LeaseId;
use handkite_core::protocol::{Action, DeviceKey, ObservedEffect, Revision};
use serde_json::{Value, json};
use std::io;

const OWNER: &str = "handkite-mcp";
pub(super) const CONTROL_TTL_MS: u64 = 120_000;

#[derive(Default)]
pub(super) struct Session {
    lease: Option<LeaseId>,
    acquisition_unknown: bool,
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
        exchange: &mut F,
    ) -> Value
    where
        F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
    {
        let Some(lease) = self.lease else {
            return tool_error("explicit control acquisition required");
        };
        let kind = action.kind();
        let response = exchange_reply(
            json!({"method": "act", "owner": OWNER, "lease": lease,
            "request": {"device": device, "expected_revision": revision, "action": action}}),
            exchange,
        );
        match response {
            Ok(Reply::ViewChanged { changed }) if changed.any() => {
                let mut result = service_error("ViewChanged");
                result["structuredContent"] = json!({"code":"ViewChanged","changed":changed});
                result
            }
            Ok(Reply::Acted {
                receipt,
                after,
                native_lock: None,
                effect,
            }) if receipt.action_kind == kind && effect != ObservedEffect::LockConfirmed => {
                if after
                    .as_ref()
                    .is_some_and(|observation| &observation.device != device)
                {
                    return uncertain_result(kind);
                }
                let mut result = text_result(json!({"receipt": receipt, "effect": effect,
                    "after": after.as_ref().map(metadata)}));
                if effect == ObservedEffect::Unconfirmed || receipt.accepted != Some(true) {
                    result["isError"] = json!(true);
                }
                if include_observation && let Some(observation) = after {
                    result["content"]
                        .as_array_mut()
                        .expect("text result has a content array")
                        .extend(observation_content(observation));
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
    let mut result = uncertain_result(kind);
    // Keep delivery unknown while exposing only the controller's fixed categories.
    // Never forward arbitrary service text, which may contain private content.
    if matches!(
        code,
        "Busy"
            | "Randomness"
            | "DeviceMismatch"
            | "StaleObservation"
            | "InvalidObservationWindow"
            | "RevisionExhausted"
            | "Adapter"
            | "Poisoned"
            | "InvalidOwner"
    ) {
        result["structuredContent"] = json!({"code": code});
    }
    result
}

fn service_error(code: &str) -> Value {
    // Never forward service detail that could contain a token or private screen content.
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_service_errors_expose_only_fixed_diagnostic_codes() {
        for code in ["StaleObservation", "InvalidObservationWindow", "Adapter"] {
            let reply = uncertain_service_result("tap", code);
            assert_eq!(reply["structuredContent"]["code"], code);
            assert_eq!(reply["isError"], true);
            let payload: Value =
                serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(payload["receipt"]["accepted"], Value::Null);
            assert_eq!(payload["delivery"], "unknown");
        }
        for code in ["private screen content", "Adapter: private detail", ""] {
            let reply = uncertain_service_result("tap", code);
            assert!(reply.get("structuredContent").is_none());
            assert!(!reply.to_string().contains("private"));
        }
    }
}
