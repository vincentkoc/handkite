use super::response::{exchange_reply, text_result};
use super::tool_error;
use crate::wire::Reply;
use handkite_core::controller::LeaseId;
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
}

fn service_error(code: &str) -> Value {
    // Never forward service detail that could contain a token or private screen content.
    tool_error(match code {
        "Busy" => "Busy: another session holds control",
        "InvalidLease" => "InvalidLease: explicit control acquisition required",
        "StaleRevision" => "StaleRevision: obtain a fresh observation before another action",
        "DeviceMismatch" => "DeviceMismatch: pinned identity verification failed",
        "LockedAction" => "LockedAction: action requires an unlocked phone",
        _ => "device service rejected the request",
    })
}
